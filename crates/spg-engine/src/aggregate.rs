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
use crate::join::AggRows;

impl crate::Engine {
    /// v7.39 (round 763, F31-C1) — expand a `*` / `alias.*` SELECT item
    /// into explicit column refs when the statement takes the aggregate
    /// path and the FROM is one plain catalog table. Returns `None`
    /// when nothing applies (the caller keeps the original statement).
    /// Joined / derived / SRF sources keep the old refusal for now.
    pub(crate) fn expand_aggregate_wildcard(
        &self,
        stmt: &SelectStatement,
    ) -> Option<SelectStatement> {
        use spg_sql::ast::SelectItem;
        if !stmt
            .items
            .iter()
            .any(|i| matches!(i, SelectItem::Wildcard | SelectItem::QualifiedWildcard(_)))
        {
            return None;
        }
        if !uses_aggregate(stmt) {
            return None;
        }
        let from = stmt.from.as_ref()?;
        if !from.joins.is_empty()
            || from.primary.unnest_expr.is_some()
            || from.primary.lateral_subquery.is_some()
            || from.primary.generate_series_args.is_some()
            || from.primary.table_fn_call.is_some()
            || from.primary.json_table.is_some()
            || from.primary.jsonb_each_text_arg.is_some()
        {
            return None;
        }
        let table = self.active_catalog().get(&from.primary.name)?;
        let alias = from
            .primary
            .alias
            .clone()
            .unwrap_or_else(|| from.primary.name.clone());
        let mut items: Vec<SelectItem> = Vec::with_capacity(stmt.items.len());
        for item in &stmt.items {
            match item {
                SelectItem::Wildcard => {
                    for c in &table.schema().columns {
                        items.push(SelectItem::Expr {
                            expr: Expr::Column(spg_sql::ast::ColumnName {
                                qualifier: None,
                                name: c.name.clone(),
                            }),
                            alias: None,
                        });
                    }
                }
                SelectItem::QualifiedWildcard(q) => {
                    if !q.eq_ignore_ascii_case(&alias) {
                        return None; // unknown qualifier — keep the old path
                    }
                    // Bare names: the single-table qualifier is
                    // redundant, and the group-expr matcher unifies
                    // bare-to-bare (a qualified ref would miss a bare
                    // GROUP BY id).
                    for c in &table.schema().columns {
                        items.push(SelectItem::Expr {
                            expr: Expr::Column(spg_sql::ast::ColumnName {
                                qualifier: None,
                                name: c.name.clone(),
                            }),
                            alias: None,
                        });
                    }
                }
                other => items.push(other.clone()),
            }
        }
        let mut out = stmt.clone();
        out.items = items;
        Some(out)
    }
}

/// True if this statement should go through the aggregate path.
pub fn uses_aggregate(stmt: &SelectStatement) -> bool {
    if stmt.group_by.is_some() || stmt.having.is_some() {
        return true;
    }
    uses_aggregate_ignoring_group_by(stmt)
}

/// v7.38.13 — the same question with the GROUP BY / HAVING short-circuit
/// removed: does an aggregate CALL appear anywhere? `baregroup` needs
/// this to tell a grouped aggregate from a GROUP BY that is a DISTINCT.
pub(crate) fn uses_aggregate_ignoring_group_by(stmt: &SelectStatement) -> bool {
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
        Expr::NamedArg { expr, .. } => contains_aggregate(expr),
        Expr::Variadic(expr) => contains_aggregate(expr),
        Expr::AggregateOrdered { .. } => true,
        Expr::Binary { lhs, rhs, .. } => contains_aggregate(lhs) || contains_aggregate(rhs),
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::BoolTest { expr, .. }
        | Expr::FieldAccess { base: expr, .. } => contains_aggregate(expr),
        Expr::Like { expr, pattern, .. } => contains_aggregate(expr) || contains_aggregate(pattern),
        Expr::Extract { source, .. } => contains_aggregate(source),
        // v4.10 subqueries + v4.12 window functions / Literal /
        // Column — all non-aggregate leaves from the regular
        // aggregate planner's POV. Window-bearing projections are
        // routed to exec_select_with_window before this runs.
        Expr::ScalarSubquery(_)
        | Expr::Exists { .. }
        | Expr::InSubquery { .. }
        | Expr::RowInSubquery { .. }
        | Expr::RowCmpSubquery { .. }
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
        Expr::ArraySlice { target, lo, hi } => {
            contains_aggregate(target)
                || lo.as_deref().is_some_and(contains_aggregate)
                || hi.as_deref().is_some_and(contains_aggregate)
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
            // PG 16+ — any_value: an arbitrary non-NULL value from
            // the group (SPG: the first seen, deterministic for
            // ordered input).
            | "any_value"
            // PG 14+ — range_agg: collect ranges into a multirange
            // (insertion order, no coalescing — matches the
            // multirange constructor contract).
            | "range_agg"
            // PG 14+ — range_intersect_agg: intersection fold.
            | "range_intersect_agg"
            // MySQL group_concat (string_agg with ',' default) +
            // SQL/XML xmlagg (separator-less concatenation).
            | "group_concat"
            | "xmlagg"
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
            | "json_agg_strict" | "jsonb_agg_strict"
            | "json_object_agg_strict" | "jsonb_object_agg_strict"
            | "json_object_agg_unique" | "jsonb_object_agg_unique"
            | "json_object_agg_unique_strict" | "jsonb_object_agg_unique_strict"
            // SQL:2016 standard spellings (PG 16+ accepts both).
            | "json_arrayagg" | "json_objectagg"
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
    // v7.39 (round 354, M12) — group_concat's SEPARATOR is lowered onto the
    // same second argument string_agg takes; without this the separator was
    // parsed and then dropped, so `SEPARATOR '|'` silently kept the default
    // comma.
    name == "group_concat"
        || name == "string_agg"
        || name.starts_with("json_object_agg")
        || name.starts_with("jsonb_object_agg")
        || name == "jsonb_object_agg"
        || name == "json_objectagg"
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
pub(crate) enum AggKind {
    CountStar,
    Count,
    Sum,
    Avg,
    Min,
    Max,
    /// PG 16+ any_value — first non-NULL value seen.
    AnyValue,
    /// PG 14+ range_agg — collect ranges into a multirange.
    RangeAgg,
    /// PG 14+ range_intersect_agg — intersection fold over ranges.
    RangeIntersectAgg,
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
/// v7.39 (round 231) — the spelling `classify_agg_name` / `update_state` /
/// `finalize` expect. PG's `every` is a standard-SQL alias for `bool_and`
/// and every accumulator keys off the latter. The GROUP BY builder folded
/// it at two of its own call sites; the window path (round 230) reached
/// `classify_agg_name` without folding and hit its panic arm, so
/// `every(x) OVER (…)` aborted the query. One entry point now, and
/// `every_aggregate_name_classifies` keeps the two name lists in step.
pub(crate) fn canonical_agg_name(name: &str) -> &str {
    if name.eq_ignore_ascii_case("every") {
        "bool_and"
    } else {
        name
    }
}

pub(crate) fn classify_agg_name(name: &str) -> AggKind {
    match name {
        "count_star" => AggKind::CountStar,
        "count" => AggKind::Count,
        "sum" => AggKind::Sum,
        "avg" => AggKind::Avg,
        "min" => AggKind::Min,
        "max" => AggKind::Max,
        "any_value" => AggKind::AnyValue,
        "range_agg" => AggKind::RangeAgg,
        "range_intersect_agg" => AggKind::RangeIntersectAgg,
        "string_agg" | "group_concat" | "xmlagg" => AggKind::StringAgg,
        "array_agg" => AggKind::ArrayAgg,
        "bool_and" => AggKind::BoolAnd,
        "bool_or" => AggKind::BoolOr,
        "stddev" | "stddev_samp" | "stddev_pop" | "variance" | "var_samp" | "var_pop" => {
            AggKind::StddevFamily
        }
        "bit_and" => AggKind::BitAnd,
        "bit_or" => AggKind::BitOr,
        "bit_xor" => AggKind::BitXor,
        "json_agg" | "jsonb_agg" | "json_arrayagg" | "json_agg_strict" | "jsonb_agg_strict" => {
            AggKind::JsonAgg
        }
        "json_object_agg"
        | "jsonb_object_agg"
        | "json_objectagg"
        | "json_object_agg_strict"
        | "jsonb_object_agg_strict"
        | "json_object_agg_unique"
        | "jsonb_object_agg_unique"
        | "json_object_agg_unique_strict"
        | "jsonb_object_agg_unique_strict" => AggKind::JsonObjectAgg,
        n if is_within_group_name(n) => AggKind::WithinGroup,
        n if is_regression_name(n) => AggKind::Regression,
        other => panic!("classify_agg_name: unknown aggregate {other}"),
    }
}

/// Per-aggregate running state.
///
/// The four `use_*` flags are independent observations about which value
/// shapes have flowed through this accumulator (a single `sum()` can see both
/// numeric and float inputs), not a discriminant — collapsing them into one
/// enum would change accumulation semantics, and a bitflags word would hide
/// which gate each fast path reads.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Default, Clone)]
pub(crate) struct AggState {
    /// The shared sum/avg running state (see `NumAcc`).
    num: NumAcc,
    extreme: Option<Value<'static>>,
    /// v7.17.0 — running collection for string_agg / array_agg.
    /// Each entry is one row's contribution (NULL preserved as
    /// `Value::Null`; string_agg's finalize step drops them, but
    /// array_agg keeps them). Pushing in insertion order matches
    /// PG behaviour when no `ORDER BY` is given inside the
    /// aggregate call.
    items: Vec<Value<'static>>,
    /// v7.39 (round 762, F31-C2) — per-item separator, parallel to
    /// `items`. PG evaluates string_agg's separator PER ROW: element
    /// i is prefixed by ITS row's separator (`string_agg(v,
    /// '<'||v||'>')` over a,b,c answers `a<b>b<c>c`; a NULL separator
    /// renders empty; a skipped-NULL value row's separator is never
    /// used). Populated only on the general path when the call has a
    /// second argument; the fused lane is literal-separator only and
    /// keeps the single `separator` snapshot below.
    item_seps: Vec<Option<String>>,
    /// v7.25 (round-17) — per-group dedupe set for DISTINCT
    /// aggregates (encoded values; NULLs never reach it because
    /// the caller's skip runs after the per-aggregate NULL rules).
    /// v7.37.4 measured `hashbrown::HashSet` as worse at this
    /// shape — the per-(group × distinct-spec) hash table alloc
    /// overhead beats the lookup-speed gain when each set is
    /// small. Sticking with `BTreeSet`; the dispatch-side enum
    /// fix in `update_state` is the R34 win.
    seen: BTreeSet<String>,
    /// v7.37.x (docker-fair DISTA attack) — fast-path BigInt seen
    /// set. The hot DISTINCT path used `encode_key_refs_into` to
    /// turn `Value::BigInt(n)` into a string key like `"I<n>|"` then
    /// inserted that into the String BTreeSet — ~100 ns of pure alloc
    /// + format churn per row × 25 k rows × 1 BigInt DISTINCT spec
    /// (the DISTA `COUNT(DISTINCT m.id)` shape) ≈ 2.5 ms of waste.
    /// Direct `BTreeSet<i64>` skips encode entirely; lookups stay
    /// O(log small) on the per-group set. Lazy-allocated — only the
    /// BigInt-DISTINCT path constructs it.
    seen_int: Option<BTreeSet<i64>>,
    /// v7.24 (round-16 A) — per-item ORDER BY key tuples, parallel
    /// to `items` (pushed under the same skip/keep conditions).
    /// Empty when the aggregate carries no internal ordering.
    /// v7.39 (round 723) — FLAT (SoA): `order_by.len()` key values per
    /// item, back to back. The per-item `Vec<Vec<Value>>` form allocated
    /// one heap Vec PER ROW just to hold (usually) one integer — ~20 ms
    /// of pure allocator traffic on the panel's 500k `string_agg(s, ','
    /// ORDER BY id)`. The key width is the spec's `order_by.len()`,
    /// which every consumer already has.
    item_keys: Vec<Value<'static>>,
    /// v7.17.0 — captured separator for string_agg: the last
    /// non-NULL text seen. v7.39 (round 762, F31-C2) — this is the
    /// CONSTANT-separator snapshot only (fused lane, group_concat
    /// default, DISTINCT fallback); the per-row truth lives in
    /// `item_seps` (the old note claimed "use the latest row's
    /// value" was PG's behaviour — measured false, PG is per-row).
    separator: Option<String>,
    /// v7.17.0 — running boolean accumulator for bool_and /
    /// bool_or / every. `None` until the first non-NULL input;
    /// at finalize None → SQL NULL.
    bool_acc: Option<bool>,
    /// v7.32 (round-29) — sum of squares for the variance / stddev
    /// family (`sum_float` carries the running sum; `count` the n).
    sum_sq: f64,
    /// v7.38 (read01) — exact accumulators for the stddev/variance family.
    /// PG computes those aggregates in NUMERIC over exact inputs (its float8
    /// overload only serves float inputs), so an f64 accumulator loses PG's
    /// exact division scale — `var_pop(1,2,3)` is `0.66666666666666666667`,
    /// not the 16-digit double. `stddev_saw_float` flips on the first
    /// float/real input and drops the family back to the f64 accumulators,
    /// whose result is then double precision, matching PG's float8 overload.
    stddev_saw_float: bool,
    stddev_sum: Option<spg_storage::bignum::BigNumeric>,
    stddev_sum_sq: Option<spg_storage::bignum::BigNumeric>,
    /// v7.39 (round 615) — the same exact Σx / Σx², accumulated in `i128`
    /// while every input is an integer and neither sum has overflowed.
    ///
    /// The `BigNumeric` pair above is exact and is what the finaliser wants,
    /// but reaching it cost NINE allocations a row on a plain INTEGER column
    /// — a boxed value per input, its square, and a fresh box for each of
    /// the two running totals — where `sum` and `avg` over the same column
    /// cost none. `i128` holds the same integers exactly: an `int4` squares
    /// to at most 4.6e18, so the running Σx² has room for 3.7e19 rows before
    /// it can overflow, and a `bigint` input that does overflow falls back
    /// below with nothing lost — the pair is folded into the BigNumeric
    /// accumulator first, so the total is the one it would have had.
    stddev_i_sum: i128,
    stddev_i_sum_sq: i128,
    stddev_i_spent: bool,
    /// v7.32 (round-29) — running accumulator for bit_and / bit_or /
    /// bit_xor. `None` until the first non-NULL input → SQL NULL.
    bit_acc: Option<i64>,
    /// v7.38 (read01, T4.4) — true once a BIGINT input is seen, so
    /// bit_and/or/xor finalize as bigint vs integer (PG input-typed).
    bit_wide: bool,
    /// v7.39 (round 254/255) — EVERY row fed to a WITHIN GROUP
    /// aggregate, NULLs included. `items` (and `count`) hold only the
    /// non-NULL values, which is right for `percentile_*` / `mode` —
    /// but PG's hypothetical-set fractions divide by the full input
    /// size: with one extra NULL row, `percent_rank(3)` moves from 2/6
    /// to 2/7 (probed live). rank / dense_rank are unaffected either
    /// way, since they only count values sorting before the
    /// hypothetical row.
    within_group_rows: usize,
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
    aux_items: Vec<Value<'static>>,
    /// v7.33 (array_agg argmax) — for a `first_ordered` spec
    /// (`(array_agg(x ORDER BY y))[1]`), the running first-by-order
    /// (sort-key tuple, value). Replaced only when a new row's key sorts
    /// strictly before the current best (ties keep the earliest row, =
    /// the stable-sort `[1]`). No items/item_keys array is built.
    first_best: Option<(Vec<Value<'static>>, Value<'static>)>,
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
    /// v7.39 (read01 orderedsetaggs.c) — the remaining direct arguments
    /// of a multi-key hypothetical-set call (`rank(5, 'x') WITHIN GROUP
    /// (ORDER BY a, b)`); one per sort key past the first. Empty
    /// everywhere else.
    direct_args_extra: Vec<Expr>,
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
    /// v7.39 (enum order knife) — member labels when the aggregate's
    /// argument is enum-typed and the aggregate orders its input
    /// (min/max): extreme comparisons use member order, not label text.
    /// Enriched once per query in `run` (spec collection is AST-only and
    /// has no catalog).
    enum_labels: Option<Vec<String>>,
    /// v7.39 (round 690) — the argument column's declared collation, for
    /// `min`/`max`. Resolved beside `enum_labels` and for the same reason:
    /// both are facts about the ARGUMENT that the comparison needs and
    /// cannot look up for itself.
    arg_collation: Option<alloc::string::String>,
    /// v7.39 (enum order knife) — per-ORDER-BY-key member labels for the
    /// ordered collection aggregates (`array_agg(x ORDER BY enum_col)`).
    /// Parallel to `order_by`; all-None when no key is enum-typed.
    order_enum_labels: Vec<Option<Vec<String>>>,
    /// v7.38.18 — per-ORDER-BY-key declared collation, for the ordered
    /// collection aggregates. Parallel to `order_by`, resolved the same
    /// way and for the same reason as `order_enum_labels` beside it.
    ///
    /// `min`/`max` have read the argument's collation since round 690
    /// (`arg_collation`), and so does the statement's own ORDER BY, but
    /// the sort INSIDE an aggregate did not: on a column declared
    /// `COLLATE "en_US.utf8"`, `SELECT x FROM t ORDER BY x` answered
    /// `apple, client, DateStyle, Zebra` while `string_agg(x, ' ' ORDER
    /// BY x)` over the same column answered `DateStyle Zebra apple
    /// client`. Two orderings of one column in one query.
    order_collations: Vec<Option<alloc::string::String>>,
}

/// Output of running the aggregate path. Schema describes one row per
/// group; rows are not yet ORDER BY-sorted (caller does it).
#[derive(Debug)]
pub struct AggResult {
    pub columns: Vec<ColumnSchema>,
    pub rows: Vec<Row<'static>>,
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
    pub synth_rows: Vec<Row<'static>>,
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
pub type CorrelatedEval<'a> =
    &'a dyn Fn(&Expr, &Row<'static>, &EvalContext<'_>) -> Result<Value<'static>, EvalError>;

/// Output of the per-group projection stage (`project_groups`): the
/// output schema, the projected rows, the synth rows kept alongside
/// them for post-LIMIT deferred evaluation, the deferred subquery
/// items, and the rewritten ORDER BY exprs (shared with the sort).
struct Projection {
    columns: Vec<ColumnSchema>,
    out_rows: Vec<Row<'static>>,
    kept_synth: Vec<Row<'static>>,
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
    rows: AggRows<'_>,
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

/// v7.39 (round 528) — a GROUP BY name that names an output column.
///
/// `SELECT date_trunc('day', ts) AS d, count(*) FROM t GROUP BY d` is the
/// canonical daily rollup, and it answered `column "d" does not exist`.
/// Both PG and MySQL take a GROUP BY identifier that matches an output
/// alias and group by the expression behind it; only grouping by a real
/// column or an ordinal worked here.
///
/// Precedence is PG's, measured: an INPUT column of that name WINS.
/// `SELECT v AS ts … GROUP BY ts` on a table that has a `ts` column
/// groups by the column, which is why PG then rejects the ungrouped `v` —
/// so the alias is consulted only when nothing else answers to the name.
fn resolve_group_by_aliases(
    keys: Vec<Expr>,
    stmt: &SelectStatement,
    schema_cols: &[ColumnSchema],
) -> Result<Vec<Expr>, EvalError> {
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        let Expr::Column(c) = &key else {
            out.push(key);
            continue;
        };
        if c.qualifier.is_some()
            || schema_cols
                .iter()
                .any(|sc| sc.name.eq_ignore_ascii_case(&c.name))
        {
            out.push(key);
            continue;
        }
        let target = stmt.items.iter().find_map(|it| match it {
            SelectItem::Expr {
                expr,
                alias: Some(a),
            } if a.eq_ignore_ascii_case(&c.name) => Some(expr),
            _ => None,
        });
        match target {
            // PG's wording for the one alias that cannot be grouped by.
            Some(e) if contains_aggregate(e) => {
                return Err(EvalError::TypeMismatch {
                    detail: alloc::string::String::from(
                        "aggregate functions are not allowed in GROUP BY",
                    ),
                });
            }
            Some(e) => out.push(e.clone()),
            // Not an alias either — leave it, so the resolver reports the
            // missing column as it always did.
            None => out.push(key),
        }
    }
    Ok(out)
}

pub(crate) fn run(
    stmt: &SelectStatement,
    rows: AggRows<'_>,
    schema_cols: &[ColumnSchema],
    table_alias: Option<&str>,
    correlated_eval: Option<CorrelatedEval<'_>>,
    // v7.39 (parallel-agg P1) — host-injected executor; None = the
    // single-threaded paths, byte-identical to pre-P1.
    runner: Option<&dyn crate::ParallelRunner>,
    // v7.39 (enum order knife) — catalog for enum member-order metadata
    // (spec collection is AST-only). None keeps every ordering textual.
    catalog: Option<&spg_storage::Catalog>,
    // v7.39 (read01 round 63) — and the engine, so a user function whose body
    // has its own FROM can run inside an aggregate's argument
    // (`string_agg(lookup(id), ',')`). The catalog alone is not enough: the body
    // is a QUERY and has to go through the real executor.
    engine: Option<&crate::Engine>,
) -> Result<AggResult, EvalError> {
    // v7.38 P0 元机制 A — fires at the top of the aggregate
    // executor with the number of input rows. Tests use this to
    // block before a hypothetical spill decision; in release it
    // expands to `let _ = (...);`.
    let __spg_row_count = rows.len();
    crate::injection_point!("aggregate_spill_trigger", &__spg_row_count);
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
    // v7.39 (round 528) — a GROUP BY name that is only an output ALIAS.
    let group_exprs = resolve_group_by_aliases(group_exprs, stmt, schema_cols)?;

    // v7.39 (round 620) — PG's strict rule, checked BEFORE the pipeline so the
    // diagnosis names what is actually wrong. Skipped under the MySQL dialect,
    // which licenses exactly what this rejects (the loose rewrite below), and
    // skipped when the grouping is by a primary key, which licenses every other
    // column of that table.
    // A GROUP BY name that resolves to nothing is reported as the missing
    // column it is, ahead of this rule — measured against PG, which answers
    // `column "nosuch" does not exist` for `SELECT v FROM t GROUP BY nosuch`
    // rather than complaining that `v` is ungrouped.
    let group_keys_all_resolve = group_exprs.iter().all(|g| match g {
        Expr::Column(c) => {
            c.qualifier.is_some()
                || schema_cols
                    .iter()
                    .any(|sc| sc.name.eq_ignore_ascii_case(&c.name))
        }
        _ => true,
    });
    let licensed = qualifiers_grouped_by_primary_key(stmt, &group_exprs, schema_cols, catalog);
    let fd_on_primary_key = !licensed.is_empty();
    // v7.39.2 — "the dialect is MySQL" used to be the whole test here,
    // so the strict rule was off even under MySQL's own default
    // `sql_mode`, which carries ONLY_FULL_GROUP_BY. It asks sql_mode
    // now. The other four dialect checks in this file ask DIFFERENT
    // questions through the same flag — column naming, HAVING aliases,
    // collation folding — and are deliberately left alone.
    if group_keys_all_resolve && !engine.is_some_and(crate::Engine::group_by_is_loose) {
        let offender = stmt
            .items
            .iter()
            .find_map(|it| match it {
                SelectItem::Expr { expr, .. } => {
                    first_ungrouped_column(expr, &group_exprs, schema_cols, &licensed)
                }
                _ => None,
            })
            .or_else(|| {
                stmt.order_by.iter().find_map(|o| {
                    first_ungrouped_column(&o.expr, &group_exprs, schema_cols, &licensed)
                })
            })
            .or_else(|| {
                stmt.having
                    .as_ref()
                    .and_then(|h| first_ungrouped_column(h, &group_exprs, schema_cols, &licensed))
            });
        if let Some(c) = offender {
            // PG qualifies the column with the alias when there is one, and
            // with the table name otherwise.
            let qual = c
                .qualifier
                .as_deref()
                .or(table_alias)
                .or_else(|| stmt.from.as_ref().map(|f| f.primary.name.as_str()))
                .unwrap_or("");
            return Err(EvalError::TypeMismatch {
                detail: alloc::format!(
                    "column \"{qual}.{}\" must appear in the GROUP BY clause or be used in an aggregate function",
                    c.name
                ),
            });
        }
    }

    // v7.39 (round 405) — MySQL's loose GROUP BY: wrap each non-grouped,
    // non-aggregated column in `any_value(col)` so the rest of the pipeline
    // treats it as an aggregate (first-seen value per group). Only under the
    // dialect and only when there is an explicit GROUP BY; PG keeps the
    // strict "must appear in GROUP BY / be aggregated" rule.
    //
    // v7.39 (round 620) — the same rewrite serves PG's functional dependency.
    // Letting the ungrouped column PAST the check above is not enough: the
    // grouped row carries only the keys and the aggregates, so `s` still has
    // nowhere to be read from and the query failed on `column "s" does not
    // exist`. Grouping by a primary key means one input row per group, so
    // "any value in the group" IS the value — the identical rewrite, reached
    // for a different and much narrower reason.
    let mysql_loose = engine.is_some_and(crate::Engine::group_by_is_loose);
    let loose_stmt;
    let stmt = if (mysql_loose || fd_on_primary_key) && !group_exprs.is_empty() {
        // The dialect claims every ungrouped column; the functional dependency
        // claims only what a grouped primary key determines.
        let claim: Option<&[alloc::string::String]> =
            if mysql_loose { None } else { Some(&licensed) };
        let mut s = stmt.clone();
        for item in &mut s.items {
            if let SelectItem::Expr { expr, .. } = item {
                let taken = core::mem::replace(expr, Expr::Literal(spg_sql::ast::Literal::Null));
                *expr = wrap_loose_group_columns(taken, &group_exprs, schema_cols, claim);
            }
        }
        for o in &mut s.order_by {
            let taken = core::mem::replace(&mut o.expr, Expr::Literal(spg_sql::ast::Literal::Null));
            o.expr = wrap_loose_group_columns(taken, &group_exprs, schema_cols, claim);
        }
        if let Some(h) = s.having.take() {
            s.having = Some(wrap_loose_group_columns(
                h,
                &group_exprs,
                schema_cols,
                claim,
            ));
        }
        loose_stmt = s;
        &loose_stmt
    } else {
        stmt
    };

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
    validate_within_group(&agg_specs, schema_cols, stmt.group_by.as_deref())?;

    // v7.38.18 (S2) — the database's collation, for the columns that
    // declare none. `None` when it is byte order, which is every
    // database written before this existed.
    let db_collation: Option<&str> = catalog
        .map(spg_storage::Catalog::db_collation)
        .filter(|d| !crate::collate::is_byte_wise(d));
    // v7.39 (round 690) — resolve the argument's declared collation for
    // `min`/`max`. This rides beside `enum_labels` in `AggSpec` but NOT
    // inside its resolver loop: that loop only runs when the catalog holds
    // at least one enum type, and a collation has nothing to do with enums.
    for spec in &mut agg_specs {
        if matches!(spec.kind, AggKind::Min | AggKind::Max)
            && let Some(Expr::Column(c)) = &spec.arg
        {
            // A bare column argument carries its collation; an expression
            // produces a new value and has none (derivation is unbuilt).
            spec.arg_collation = schema_cols
                .iter()
                .find(|sc| sc.name.eq_ignore_ascii_case(&c.name))
                .and_then(|sc| sc.collation_name.clone())
                // v7.38.18 (S2) — the database's when the column
                // declares none. `C` filters out below, so nothing moves
                // for a database that has not asked for a locale.
                .or_else(|| db_collation.map(alloc::string::String::from))
                .filter(|n| crate::collate::is_supported(n));
        }
    }

    // v7.38.18 — the same fact for each ORDER BY key of an ordered
    // collection aggregate. Outside the enum resolver below for the
    // reason the loop above is: that one only runs when the catalog
    // holds an enum type, and a collation has nothing to do with enums.
    for spec in &mut agg_specs {
        if spec.order_by.is_empty() {
            continue;
        }
        spec.order_collations = spec
            .order_by
            .iter()
            .map(|o| {
                // A bare column key carries its collation; an expression
                // produces a new value and has none, the same limit
                // `min`/`max` has over an expression argument.
                let Expr::Column(c) = &o.expr else {
                    return None;
                };
                schema_cols
                    .iter()
                    .find(|sc| sc.name.eq_ignore_ascii_case(&c.name))
                    .and_then(|sc| sc.collation_name.clone())
                    .or_else(|| db_collation.map(alloc::string::String::from))
                    .filter(|n| crate::collate::is_supported(n))
            })
            .collect();
    }

    // v7.39 (enum order knife) — resolve enum member-order metadata once
    // per query: min/max extremes and ordered-collection sort keys over
    // enum-typed expressions compare by member order (PG enumsortorder).
    if let Some(cat) = catalog
        && !cat.enum_types().is_empty()
    {
        for spec in &mut agg_specs {
            // v7.39 (round 258) — min/max have always needed the argument's
            // enum labels; a DISTINCT aggregate now does too, because its
            // dedup sort must follow MEMBER order (round 257 added the sort
            // and, deriving labels only here, sorted enum columns by text).
            if (matches!(spec.kind, AggKind::Min | AggKind::Max) || spec.distinct)
                && let Some(arg) = &spec.arg
            {
                spec.enum_labels = crate::eval::expr_enum_labels(arg, schema_cols, catalog)
                    .map(<[String]>::to_vec);
            }
            if !spec.order_by.is_empty() {
                spec.order_enum_labels = spec
                    .order_by
                    .iter()
                    .map(|o| {
                        crate::eval::expr_enum_labels(&o.expr, schema_cols, catalog)
                            .map(<[String]>::to_vec)
                    })
                    .collect();
            }
        }
    }

    // (1) Stream the WHERE-filtered rows into insertion-ordered group state.
    let order = accumulate_groups(
        rows,
        &group_exprs,
        &agg_specs,
        schema_cols,
        table_alias,
        correlated_eval,
        runner,
        catalog,
        engine,
    )?;

    // (2) Build the synthetic per-group schema and finalise each group's row.
    let synth_schema = build_synth_schema(
        rows,
        &group_exprs,
        &agg_specs,
        schema_cols,
        table_alias,
        catalog,
        engine,
    )?;
    let synth_rows = finalize_synth_rows(
        &order,
        &agg_specs,
        &synth_schema,
        rows,
        schema_cols,
        table_alias,
        catalog,
        engine,
        runner,
    )?;

    // v7.37.x (mailrs Track A 100k attack) — defer the bound
    // per-item SELECT projection on the synth rows until AFTER
    // sort + LIMIT truncation. On a `GROUP BY t ORDER BY agg DESC
    // LIMIT 50` with 20 000 groups (the mailrs minimal 100k shape)
    // pre-defer ran 20 000 × N_items compiled-VM evals + Row
    // allocations before discarding 99.75 % at the sort truncation
    // step. HAVING still runs inline on every group because it
    // filters BEFORE the LIMIT; we only skip the SELECT-list eval.
    //
    // v7.37 (round 998) — and so a HAVING no longer stands the deferral
    // down. It used to, which cost the mailrs Track A query 11.9 ms of
    // 83. Neither clause is expensive alone: HAVING costs 5.0 ms without
    // an ORDER BY and 16.9 with one, and an ORDER BY costs MINUS 8.6 ms
    // without a HAVING, because ORDER BY + LIMIT is what switches this
    // deferral on. The residue of 11.9 ms belonged to neither and
    // appeared only together.
    //
    // What named it: the interaction tracks what the aggregates COST
    // rather than how many there are — one expensive aggregate
    // reproduces it as fully as twelve cheap ones — and it does not move
    // when the LIMIT changes. Both follow from projecting all 20 000
    // groups instead of the 50 that survive truncation.
    //
    // Safe because the clause above runs first: HAVING filters into
    // `kept_synth` BEFORE this branch, the sort truncates that survivor
    // list, and the completion projects from it. HAVING is rewritten
    // against the synthetic group schema, so it never reads a projected
    // item.
    //
    // v7.37 (round 997) — a set-returning item must NOT defer. The
    // deferred completion at the end of this function evaluates each item
    // scalarly; the expansion that turns one group into one row per
    // element lives in the branch the deferral skips. So a deferred
    // `unnest(...)` in the select list came back as
    // `function unnest(integer[]) does not exist` — the exact error round
    // 621 had fixed, reintroduced for the shapes that qualify to defer.
    // Differential against PG18.4: the same query answered correctly
    // without LIMIT, with LIMIT >= the group count, and — at the time —
    // with a HAVING, those being the cases where the deferral was off.
    // Round 998 removed the HAVING one from that list, which is why this
    // guard carries the SRF rule on its own now.
    let any_srf_item = stmt.items.iter().any(|i| match i {
        SelectItem::Expr { expr, .. } => crate::select::top_level_srf_kind(expr).is_some(),
        _ => false,
    });
    let defer_projection = !stmt.order_by.is_empty()
        && !stmt.distinct
        && !stmt.limit_with_ties
        && !any_srf_item
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
        catalog,
        engine.is_some_and(|e| e.backslash_escapes),
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
            &columns,
            &stmt.order_by,
            &order_rewritten,
            kept_synth,
            out_rows,
            correlated_eval,
            keep_n,
            catalog,
            engine.is_some_and(|e| e.backslash_escapes),
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
        let mut synth_ctx = EvalContext::new(&synth_schema, None);
        if let Some(cat) = catalog {
            synth_ctx = synth_ctx.with_catalog(cat);
        }
        let mut stack: Vec<Value<'static>> = Vec::new();
        for (idx, srow) in kept_synth.iter().enumerate() {
            let mut values: Vec<Value<'static>> = Vec::with_capacity(columns.len());
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

    // v7.37 (round 999) — SELECT DISTINCT over a GROUP BY query.
    //
    // Every other path deduplicates: the scan paths, the window path and
    // the set operations all call `dedup_rows`. This one never did, so
    // `SELECT DISTINCT count(*) FROM t GROUP BY g` returned one row per
    // GROUP — 200 where PG18.4 returns 1, all of them the same value.
    // Not an error, not a missing column: 199 extra rows, silently.
    //
    // The gate on the top-K sink above says it in as many words — "no
    // DISTINCT (would need post-dedup, can't truncate during sort)" — so
    // the sink correctly declines to truncate, and the post-dedup it
    // names was never written. This is it.
    //
    // After the ORDER BY, like the window path: duplicate rows carry
    // identical sort keys, so removing them cannot disturb the order.
    // Before the LIMIT, which the caller applies, because PG deduplicates
    // and then counts.
    //
    // Only `out_rows` needs it: `deferred` is empty whenever DISTINCT is
    // set (`defer_enabled` requires `!stmt.distinct`), so nothing indexes
    // into `kept_synth` alongside these rows.
    if stmt.distinct {
        // v7.38.14 — masked, not dialect-only. `SELECT DISTINCT` over a
        // GROUP BY result folded every text position regardless of what
        // the column declared, which is the defect 3b494b6e closed on the
        // main scan path. The output schema is in scope here and carries
        // the collation, so the mask needs no new plumbing.
        out_rows = crate::select::dedup_rows(
            out_rows,
            crate::select::FoldSpec::of_masks(
                engine.is_some_and(|e| e.backslash_escapes),
                &crate::select::fold_mask_of_columns(&columns),
                &crate::select::pad_mask_of_columns(&columns),
            ),
        );
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
/// v7.39 (round 255) — PG's name for an expression's type in an
/// ordered-set signature error. Only a CAST / COLUMN is trusted (the
/// round-237 lesson: `describe_expr` reports a binary operator as its
/// left operand's type); an untyped literal is PG's own `unknown`, and
/// anything else falls back to `unknown` rather than guessing.
fn ordered_set_arg_type_name(e: &Expr, columns: &[ColumnSchema]) -> String {
    if matches!(
        e,
        Expr::Literal(spg_sql::ast::Literal::String(_))
            | Expr::Literal(spg_sql::ast::Literal::Null)
    ) {
        return String::from("unknown");
    }
    match e {
        Expr::Cast { .. } | Expr::Column(_) | Expr::Literal(_) => {
            crate::describe::describe_expr(e, columns).map_or_else(
                || String::from("unknown"),
                |s| crate::conversions::pg_type_name_for_error(s.ty),
            )
        }
        _ => String::from("unknown"),
    }
}

/// v7.39 (round 255) — PG resolves an ordered-set / hypothetical-set
/// call as ONE function whose signature is `(direct args…, WITHIN GROUP
/// args…)`; anything that does not match a declared overload is a plain
/// `function f(…) does not exist` (42883), not a bespoke message. Probed
/// live: `percentile_cont(numeric, text)`, `rank(integer, integer,
/// text)`, `mode(integer, integer)`.
fn ordered_set_signature_error(name: &str, spec: &AggSpec, columns: &[ColumnSchema]) -> EvalError {
    let mut parts: Vec<String> = Vec::new();
    if let Some(d) = &spec.direct_arg {
        parts.push(ordered_set_arg_type_name(d, columns));
    }
    for d in &spec.direct_args_extra {
        parts.push(ordered_set_arg_type_name(d, columns));
    }
    for o in &spec.order_by {
        parts.push(ordered_set_arg_type_name(&o.expr, columns));
    }
    EvalError::TypeMismatch {
        detail: format!("function {name}({}) does not exist", parts.join(", ")),
    }
}

fn validate_within_group(
    agg_specs: &[AggSpec],
    columns: &[ColumnSchema],
    group_by: Option<&[Expr]>,
) -> Result<(), EvalError> {
    // v7.39 (round 765, F31-D2) — PG requires an ordered-set
    // aggregate's DIRECT arguments to use only grouped columns
    // (`percentile_cont(x) WITHIN GROUP (ORDER BY x)` refuses with
    // "column … must appear in the GROUP BY clause", DETAIL "Direct
    // arguments of an ordered-set aggregate must use only grouped
    // columns", PG18-measured); SPG evaluated the first row's value
    // and answered.
    fn first_ungrouped(e: &Expr, group_by: Option<&[Expr]>) -> Option<String> {
        let mut found: Option<String> = None;
        let mut subs: Vec<&SelectStatement> = Vec::new();
        crate::visit_expr_columns_and_subqueries(
            e,
            &mut |c| {
                if found.is_some() {
                    return;
                }
                let grouped = group_by.is_some_and(|gs| {
                    gs.iter().any(|g| match g {
                        Expr::Column(gc) => gc.name.eq_ignore_ascii_case(&c.name),
                        _ => false,
                    })
                });
                // The visitor's exotic-node BAIL marker is an empty
                // name — not a real column; skip it (refusing on it
                // would reject constant shapes like ARRAY[…] casts).
                if !grouped && !c.name.is_empty() {
                    found = Some(match &c.qualifier {
                        Some(q) => format!("{q}.{}", c.name),
                        None => c.name.clone(),
                    });
                }
            },
            &mut |s| subs.push(s),
        );
        found
    }
    for spec in agg_specs {
        if !is_within_group_name(&spec.name) {
            continue;
        }
        for d in spec.direct_arg.iter().chain(spec.direct_args_extra.iter()) {
            if let Some(col) = first_ungrouped(d, group_by) {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "column \"{col}\" must appear in the GROUP BY clause or be used in an aggregate function"
                    ),
                });
            }
        }
    }
    // v7.32 (round-29) — WITHIN GROUP aggregates require the clause (PG
    // raises a hard error otherwise rather than silently degrading), and
    // SPG supports the single-sort-key form only.
    for spec in agg_specs {
        if is_within_group_name(&spec.name) {
            if spec.order_by.is_empty() {
                // v7.39 (round 704) — the hypothetical-set names double as
                // WINDOW functions, and PG resolves the bare zero-argument
                // spelling to the window reading: `SELECT rank() FROM t` is
                // `window function rank requires an OVER clause` there, not
                // a WITHIN GROUP complaint. With a direct argument the
                // ordered-set reading is the one the caller meant, and the
                // WITHIN GROUP wording stands.
                if spec.direct_arg.is_none() && is_hypothetical_set_name(&spec.name) {
                    return Err(EvalError::TypeMismatch {
                        detail: format!("window function {} requires an OVER clause", spec.name),
                    });
                }
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
            // …and mode() takes NONE: `mode(1)` used to be accepted with
            // the argument silently dropped.
            if spec.name == "mode" && spec.direct_arg.is_some() {
                return Err(ordered_set_signature_error(&spec.name, spec, columns));
            }
            // v7.39 (read01 orderedsetaggs.c) — the hypothetical-set
            // family supports the multi-key form: one direct argument
            // per sort key (PG resolves a mismatch as a missing
            // function overload; its HINT carries the real rule).
            let hypothetical = matches!(
                spec.name.as_str(),
                "rank" | "dense_rank" | "percent_rank" | "cume_dist"
            );
            // Only the hypothetical-set family takes a multi-key sort
            // spec, and then it needs exactly one direct argument per
            // key. PG reports every mismatch as a missing overload.
            if hypothetical {
                if 1 + spec.direct_args_extra.len() != spec.order_by.len() {
                    return Err(ordered_set_signature_error(&spec.name, spec, columns));
                }
            } else if spec.order_by.len() > 1 || !spec.direct_args_extra.is_empty() {
                // `percentile_cont(0.5, 0.6)` and `mode(1)` used to be
                // silently accepted (the extra arguments were dropped and
                // the aggregate answered anyway).
                return Err(ordered_set_signature_error(&spec.name, spec, columns));
            }
            // v7.39 (round 255) — `percentile_cont` interpolates, so PG
            // declares it only over the numeric tower and interval
            // (probed: text / date / timestamp / bool are refused, while
            // `percentile_disc` and `mode` take any sortable type). SPG
            // answered NULL for the refused types. Judged from the
            // STATICALLY known type only — an unknown one is let through
            // (round 237: refusing a legal query is worse than missing an
            // illegal one).
            if spec.name == "percentile_cont"
                && let Some(o) = spec.order_by.first()
                && matches!(o.expr, Expr::Cast { .. } | Expr::Column(_))
                && let Some(sch) = crate::describe::describe_expr(&o.expr, columns)
                && !matches!(
                    sch.ty,
                    spg_storage::DataType::SmallInt
                        | spg_storage::DataType::Int
                        | spg_storage::DataType::BigInt
                        | spg_storage::DataType::Float
                        | spg_storage::DataType::Real
                        | spg_storage::DataType::Numeric { .. }
                        | spg_storage::DataType::Interval
                )
            {
                return Err(ordered_set_signature_error(&spec.name, spec, columns));
            }
        }
    }
    Ok(())
}

/// (1) Stream the WHERE-filtered rows, group by the GROUP BY value
/// tuple, and update per-group aggregate state. Returns the groups in
/// insertion order. See `run` for the bind-once fast path rationale.
/// v7.39 (round 665) — the running numeric state a sum/avg keeps, in ONE
/// place.
///
/// It used to live in four independently written copies: `FusedAcc`'s own
/// fields, `AggState`'s own fields, and twice more as loose locals inside
/// `accumulate_groups`. `FusedAcc`'s doc comment described that openly —
/// "field-for-field the same running state the single-spec sum/avg fast
/// path keeps in locals" — so the duplication was deliberate manual
/// inlining, not drift.
///
/// The cost was not abstract. Round 664 measured it: adding one guard to
/// the sum/avg family meant editing FOUR sites, and three of the four were
/// found only by running a different SQL shape and watching the wrong
/// answer come back. Reading the code did not reveal them, because the
/// three parallel loops in the fused block are not symmetric — the middle
/// one is a `length()` shortcut that accumulates nothing numeric.
///
/// `count` deliberately stays outside: `count(*)` keeps it too, and it is
/// not part of the numeric running state.
#[derive(Debug, Default, Clone)]
struct NumAcc {
    sum_int: i64,
    sum_float: f64,
    use_float: bool,
    float_not_real: bool,
    sum_num_scaled: i128,
    sum_num_kind: spg_storage::NumericKind,
    sum_num_scale: u16,
    /// v7.39 (read01 numeric.c) — bignum spill; see `SumBig`.
    sum_big: SumBig,
    use_numeric: bool,
    sum_iv_months: i64,
    sum_iv_days: i64,
    sum_iv_micros: i128,
    use_interval: bool,
    sum_money: i128,
    use_money: bool,
    /// Inside the struct, not beside it. Measured: splitting it out gave
    /// `acc_cell` two base pointers where the copy it replaced had one,
    /// and `sum(int)` over 500k rows lost ~8% (paired, n=12, p=0.04).
    /// `count(*)` reading `st.num.count` is a small price for that.
    count: i64,
}

#[allow(clippy::too_many_lines, clippy::type_complexity)]
/// v7.37.16 — per-spec accumulator for the fused multi-spec fast path.
/// Field-for-field the same running state the single-spec sum/avg fast
/// path keeps in locals; finalized into `AggState` identically.
#[derive(Default, Clone)]
struct FusedAcc {
    /// The shared sum/avg running state (see `NumAcc`).
    num: NumAcc,
    /// v7.39 (round 568/569) — the min/max lane. `min` and `max` were
    /// the only ordinary aggregates the fused layout did not accept, so
    /// they fell to the generic per-spec machinery and cost DOUBLE a
    /// `sum` over the same scan (500k INTs: sum 13.4 ms, min 26.5,
    /// max 27.6, while PG18 is flat at 8.2 for all three). They also
    /// missed the shard-parallel scan the fused path runs.
    extreme: Option<Value<'static>>,
    /// Which way this accumulator's comparison goes, so a shard merge
    /// does not need to be told.
    extreme_max: bool,
    extreme_mysql: bool,
    /// v7.39 (round 690) — the argument's declared collation, so a
    /// shard merge compares the two extremes the same way the scan did.
    extreme_coll: Option<alloc::string::String>,
    /// v7.39 (round 724) — the collection lanes: string_agg / array_agg
    /// items in ROW order (shard merge concatenates in shard order,
    /// which IS row order), plus the flat ORDER BY keys (round 723's
    /// layout). The finalize sort/join is the existing AggState path.
    items: Vec<Value<'static>>,
    item_keys: Vec<Value<'static>>,
}

/// v7.39 (round 569) — a fresh accumulator per op, carrying each one's
/// comparison direction so `merge_fused` stays a two-argument fold.
fn fused_accs(ops: &[FusedOp], mysql: bool) -> Vec<FusedAcc> {
    ops.iter()
        .map(|op| {
            let mut a = FusedAcc::default();
            if let FusedOp::Extreme { max, coll, .. } | FusedOp::ExtremeExpr { max, coll, .. } = op
            {
                a.extreme_max = *max;
                a.extreme_mysql = mysql;
                a.extreme_coll = coll.clone();
            }
            a
        })
        .collect()
}

/// v7.39 (parallel-agg P3) — the fused-op layout shared by the
/// single-group fast path and the parallel GROUP BY fast path.
/// `spec_src[i]`: None = count(*) (finalize from the group row
/// count); Some(slot) = unique_ops[slot]'s accumulator.
enum FusedOp {
    CountCol(usize),
    AccCol(usize),
    /// v7.39 (round 569) — min/max over a bound column.
    /// v7.39 (round 690) — `coll` is the column's declared collation.
    /// Unlike an enum's member order (which sends the spec to the
    /// generic path), a collation rides along, so a collated column
    /// keeps the fused lane's shard-parallel scan.
    Extreme {
        pos: usize,
        max: bool,
        coll: Option<alloc::string::String>,
    },
    /// v7.39 (round 716, S07) — the same three shapes over a COMPILED
    /// argument expression. `count(least(id, 0))` used to fall off this
    /// lane entirely — `fused_layout` only accepted bound columns — and
    /// landed in the SERIAL generic loop, which is where the whole 7.6×
    /// against PG lived: PG runs the identical cell as a parallel seq
    /// scan. The payload is the SPEC INDEX whose `arg_compiled` program
    /// to run; the accumulator lanes are the ones the column ops use.
    CountExpr(usize),
    AccExpr(usize),
    ExtremeExpr {
        spec: usize,
        max: bool,
        coll: Option<alloc::string::String>,
    },
    /// v7.39 (round 724) — string_agg / array_agg over a bound column,
    /// optional bound ORDER BY keys. The payload is the spec index; the
    /// scan reads arg_pos / order_pos through it. Collection was the
    /// last per-row aggregate stuck on the serial generic loop — 32 ms
    /// single-threaded on the panel's 500k string_agg where PG runs a
    /// parallel plan.
    Collect {
        spec: usize,
        string_kind: bool,
    },
}

/// Returns the (spec_src, unique_ops) layout when EVERY aggregate
/// spec is fused-eligible (count*/count/sum/avg over bound columns,
/// no FILTER/DISTINCT/arg2/ORDER), else None.
fn fused_layout(
    agg_specs: &[AggSpec],
    arg_pos: &[Option<usize>],
    // v7.39 (round 716) — a compiled argument keeps a spec on the fused
    // lane now; a bound column still takes the (cheaper) column op.
    arg_compiled: &[Option<eval::CompiledExpr>],
    // v7.39 (round 724) — bound ORDER BY key positions, for Collect.
    order_pos: &[Vec<Option<usize>>],
    arg2_literal_val: &[Option<Value<'static>>],
) -> Option<(Vec<Option<usize>>, Vec<FusedOp>)> {
    if agg_specs.is_empty() {
        return None;
    }
    let has_arg = |i: usize| arg_pos[i].is_some() || arg_compiled[i].is_some();
    // v7.39 (round 724) — a collection spec: bound argument, literal
    // separator (string_agg), every ORDER BY key a bound column. The
    // finalize path (sort + join) is the ordinary AggState one, so
    // multi-key and DESC orders are the finalizer's business, not ours.
    let collectible = |i: usize, s: &AggSpec| -> bool {
        !s.distinct
            && s.filter.is_none()
            && !s.first_ordered
            && arg_pos[i].is_some()
            && s.order_by
                .iter()
                .enumerate()
                .all(|(k, _)| order_pos[i].get(k).copied().flatten().is_some())
            && match s.name.as_str() {
                "string_agg" => matches!(&arg2_literal_val[i], Some(Value::Text(_))),
                "array_agg" => s.arg2.is_none() && s.enum_labels.is_none(),
                _ => false,
            }
    };
    let eligible = agg_specs.iter().enumerate().all(|(i, s)| {
        collectible(i, s)
            || (s.filter.is_none()
                && s.arg2.is_none()
                && s.order_by.is_empty()
                && !s.distinct
                && !s.first_ordered
                && match s.name.as_str() {
                    "count_star" => s.arg.is_none(),
                    "count" | "sum" | "avg" => has_arg(i),
                    // v7.39 (round 569) — an enum argument compares by
                    // catalog member order, which the fused lane does not
                    // carry; those keep the generic path.
                    "min" | "max" => has_arg(i) && s.enum_labels.is_none(),
                    _ => false,
                })
    });
    if !eligible {
        return None;
    }
    let mut unique_ops: Vec<FusedOp> = Vec::new();
    // Compiled dedupe key = the source Expr (same rule the executor-time
    // CSE uses): two specs share a slot only when their argument TREES
    // are equal, which `fully_compilable`'s purity makes sufficient.
    let same_arg = |j: usize, i: usize| agg_specs[j].arg == agg_specs[i].arg;
    let spec_src: Vec<Option<usize>> = agg_specs
        .iter()
        .enumerate()
        .map(|(i, s)| match s.name.as_str() {
            "count_star" => None,
            // Collection ops never share slots (each keeps its own
            // items), so no dedupe probe.
            "string_agg" | "array_agg" => {
                unique_ops.push(FusedOp::Collect {
                    spec: i,
                    string_kind: s.name.as_str() == "string_agg",
                });
                Some(unique_ops.len() - 1)
            }
            "min" | "max" => {
                let max = s.name.as_str() == "max";
                let slot = if let Some(p) = arg_pos[i] {
                    unique_ops
                        .iter()
                        .position(|o| {
                            matches!(o, FusedOp::Extreme { pos, max: m, coll }
                                if *pos == p && *m == max && *coll == s.arg_collation)
                        })
                        .unwrap_or_else(|| {
                            unique_ops.push(FusedOp::Extreme {
                                pos: p,
                                max,
                                coll: s.arg_collation.clone(),
                            });
                            unique_ops.len() - 1
                        })
                } else {
                    unique_ops
                        .iter()
                        .position(|o| {
                            matches!(o, FusedOp::ExtremeExpr { spec, max: m, coll }
                                if same_arg(*spec, i) && *m == max && *coll == s.arg_collation)
                        })
                        .unwrap_or_else(|| {
                            unique_ops.push(FusedOp::ExtremeExpr {
                                spec: i,
                                max,
                                coll: s.arg_collation.clone(),
                            });
                            unique_ops.len() - 1
                        })
                };
                Some(slot)
            }
            "count" => {
                let slot = if let Some(p) = arg_pos[i] {
                    unique_ops
                        .iter()
                        .position(|o| matches!(o, FusedOp::CountCol(q) if *q == p))
                        .unwrap_or_else(|| {
                            unique_ops.push(FusedOp::CountCol(p));
                            unique_ops.len() - 1
                        })
                } else {
                    unique_ops
                        .iter()
                        .position(|o| matches!(o, FusedOp::CountExpr(j) if same_arg(*j, i)))
                        .unwrap_or_else(|| {
                            unique_ops.push(FusedOp::CountExpr(i));
                            unique_ops.len() - 1
                        })
                };
                Some(slot)
            }
            _ => {
                let slot = if let Some(p) = arg_pos[i] {
                    unique_ops
                        .iter()
                        .position(|o| matches!(o, FusedOp::AccCol(q) if *q == p))
                        .unwrap_or_else(|| {
                            unique_ops.push(FusedOp::AccCol(p));
                            unique_ops.len() - 1
                        })
                } else {
                    unique_ops
                        .iter()
                        .position(|o| matches!(o, FusedOp::AccExpr(j) if same_arg(*j, i)))
                        .unwrap_or_else(|| {
                            unique_ops.push(FusedOp::AccExpr(i));
                            unique_ops.len() - 1
                        })
                };
                Some(slot)
            }
        })
        .collect();
    Some((spec_src, unique_ops))
}

/// v7.39 (parallel-agg P1) — fold shard accumulator `b` into `a`.
/// Every FusedAcc field is a running sum plus a type-witness flag, so
/// the merge is field-wise addition with `numeric_add` aligning the
/// decimal scales. Merging in shard order keeps float summation
/// deterministic for a given shard count (PG's parallel aggregate
/// makes the same no-serial-equivalence tradeoff for floats).
fn merge_fused(a: &mut FusedAcc, b: &mut FusedAcc) {
    // v7.39 (round 569) — fold the shard's extreme in the direction this
    // accumulator was built for.
    if let Some(be) = &b.extreme {
        let take = match &a.extreme {
            None => true,
            Some(ae) => {
                let ord = extreme_cmp_in(None, a.extreme_coll.as_deref(), be, ae, a.extreme_mysql);
                if a.extreme_max {
                    ord == core::cmp::Ordering::Greater
                } else {
                    ord == core::cmp::Ordering::Less
                }
            }
        };
        if take {
            a.extreme = Some(be.clone());
        }
    }
    a.num.count += b.num.count;
    a.num.sum_int += b.num.sum_int;
    a.num.sum_float += b.num.sum_float;
    a.num.use_float |= b.num.use_float;
    a.num.float_not_real |= b.num.float_not_real;
    if b.num.use_numeric {
        // v7.39 (read01 numeric.c) — fold the shard's bignum spill first,
        // then its i128 lane (zero if the shard promoted).
        if let Some(bb) = &b.num.sum_big {
            sum_add_bignum(
                &mut a.num.sum_num_scaled,
                &mut a.num.sum_num_scale,
                &mut a.num.sum_big,
                bb,
            );
        }
        sum_add_exact(
            &mut a.num.sum_num_scaled,
            &mut a.num.sum_num_scale,
            &mut a.num.sum_big,
            b.num.sum_num_scaled,
            b.num.sum_num_scale,
        );
        a.num.sum_num_kind = fold_sum_kind(a.num.sum_num_kind, b.num.sum_num_kind);
        a.num.use_numeric = true;
    }
    a.num.sum_iv_months += b.num.sum_iv_months;
    a.num.sum_iv_days += b.num.sum_iv_days;
    a.num.sum_iv_micros += b.num.sum_iv_micros;
    a.num.use_interval |= b.num.use_interval;
    a.num.sum_money += b.num.sum_money;
    a.num.use_money |= b.num.use_money;
    // v7.39 (round 724) — collection lanes concatenate; shard order is
    // row order. The merge takes `b` by reference (both call sites), so
    // this clones — the per-shard vectors are moved into place only at
    // fill time.
    a.items.extend(core::mem::take(&mut b.items));
    a.item_keys.extend(core::mem::take(&mut b.item_keys));
}

/// v7.39 — write fused accumulators into the per-spec AggStates
/// (shared by the single-group and parallel-GROUP-BY fast paths).
/// `group_rows` finalizes count(*) specs.
/// v7.39 (round 724) — one row's contribution to a fused Collect op.
/// Mirrors `update_state`'s StringAgg / ArrayAgg arms: string_agg skips
/// NULL and renders through the shared helper (a non-renderable type
/// errors with the same sentence); array_agg keeps NULL elements.
fn collect_cell(
    a: &mut FusedAcc,
    row: &crate::join::RowRef<'_>,
    pos: usize,
    key_pos: &[Option<usize>],
    string_kind: bool,
) -> Result<(), EvalError> {
    let v = row.get(pos).unwrap_or(&Value::Null);
    if string_kind {
        if matches!(v, Value::Null) {
            return Ok(());
        }
        let Some(item) = render_string_agg_item(v) else {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "string_agg requires text value, got {}",
                    crate::conversions::pg_type_name_for_error_opt(v.data_type())
                ),
            });
        };
        a.items.push(item);
    } else {
        a.items.push(v.clone().into_owned());
    }
    a.num.count += 1;
    for kp in key_pos {
        let kv = row
            .get(kp.expect("layout-gated bound key"))
            .cloned()
            .map(Value::into_owned)
            .unwrap_or(Value::Null);
        a.item_keys.push(kv);
    }
    Ok(())
}

/// The string_agg item rendering, shared by `update_state` and the
/// round-724 fused Collect op — one place, so the two paths cannot
/// drift. Text collects as-is; other scalars coerce to their text
/// rendering (MySQL group_concat semantics — also matches PG's
/// cast-then-aggregate idiom for `string_agg(v::text, sep)`).
fn render_string_agg_item(v: &Value<'_>) -> Option<Value<'static>> {
    match v {
        Value::Text(s) => Some(Value::text(s.clone())),
        // v7.39 (round 626, S05b/F29) — CHAR(n). PG aggregates a
        // bpchar column (`string_agg(c, ',')` -> text) and SPG said
        // "string_agg requires text value, got character". The text
        // form of a bpchar drops its padding, which is what PG's
        // own bpchar->text cast does.
        Value::BpChar(s) => Some(Value::text(s.trim_end_matches(' ').to_string())),
        // v7.39 (read01 round 111) — xmlagg feeds xml values through this
        // shared StringAgg path; render the fragment's text (it joins
        // separator-less into the concatenated document).
        Value::Xml(s) => Some(Value::text(s.to_string())),
        Value::Int(n) => Some(Value::text(n.to_string())),
        Value::BigInt(n) => Some(Value::text(n.to_string())),
        Value::SmallInt(n) => Some(Value::text(n.to_string())),
        Value::Float(f) => Some(Value::text(f.to_string())),
        Value::Bool(b) => Some(Value::text(if *b { "1" } else { "0" })),
        _ => None,
    }
}

fn fill_states_from_fused(
    states: &mut [AggState],
    spec_src: &[Option<usize>],
    accs: &mut [FusedAcc],
    group_rows: i64,
    // v7.39 (round 724) — string_agg's literal separator, per spec.
    arg2_literal_val: &[Option<Value<'static>>],
) {
    for (i, src) in spec_src.iter().enumerate() {
        let state = &mut states[i];
        match src {
            None => state.num.count = group_rows,
            Some(slot) => {
                // Collection lanes MOVE (they are per-spec, never
                // shared; see the layout's no-dedupe rule).
                {
                    let a = &mut accs[*slot];
                    if !a.items.is_empty() {
                        state.items = core::mem::take(&mut a.items);
                        state.item_keys = core::mem::take(&mut a.item_keys);
                    }
                }
                if let Some(Value::Text(sep)) = &arg2_literal_val[i] {
                    state.separator = Some(sep.to_string());
                }
                let a = &accs[*slot];
                state.num.count = a.num.count;
                state.num.sum_int = a.num.sum_int;
                state.num.sum_float = a.num.sum_float;
                state.num.use_float = a.num.use_float;
                state.num.float_not_real = a.num.float_not_real;
                state.num.sum_num_scaled = a.num.sum_num_scaled;
                state.num.sum_num_kind = a.num.sum_num_kind;
                state.num.sum_num_scale = a.num.sum_num_scale;
                state.num.sum_big = a.num.sum_big.clone();
                state.num.use_numeric = a.num.use_numeric;
                state.num.sum_iv_months = a.num.sum_iv_months;
                state.num.sum_iv_days = a.num.sum_iv_days;
                state.num.sum_iv_micros = a.num.sum_iv_micros;
                state.num.use_interval = a.num.use_interval;
                state.num.sum_money = a.num.sum_money;
                state.num.use_money = a.num.use_money;
                if a.extreme.is_some() {
                    state.extreme = a.extreme.clone();
                }
            }
        }
    }
}

/// v7.39 (read01 numeric.c) — the bignum spill lane of the NUMERIC sum
/// tri-state (i128 mantissa + scale + optional BigNumeric). `None` until the
/// i128 lane would overflow; from then on the sum lives in the spill and the
/// i128 lane stays frozen at zero (PG's sum(numeric) never saturates).
type SumBig = Option<alloc::boxed::Box<spg_storage::bignum::BigNumeric>>;

/// Add an exact NUMERIC (mantissa × 10^-scale) into the sum tri-state.
fn sum_add_exact(
    scaled: &mut i128,
    scale: &mut u16,
    big: &mut SumBig,
    add_scaled: i128,
    add_scale: u16,
) {
    use spg_storage::bignum::BigNumeric;
    if let Some(b) = big {
        **b = b.add(&BigNumeric::from_i128(add_scaled, add_scale));
        return;
    }
    match crate::numeric::numeric_add_checked(*scaled, *scale, add_scaled, add_scale) {
        Some((s, sc)) => {
            *scaled = s;
            *scale = sc;
        }
        None => {
            *big = Some(alloc::boxed::Box::new(
                BigNumeric::from_i128(*scaled, *scale)
                    .add(&BigNumeric::from_i128(add_scaled, add_scale)),
            ));
            *scaled = 0;
            *scale = 0;
        }
    }
}

/// Add a BigNumeric input into the sum tri-state (promotes immediately).
fn sum_add_bignum(
    scaled: &mut i128,
    scale: &mut u16,
    big: &mut SumBig,
    b_in: &spg_storage::bignum::BigNumeric,
) {
    use spg_storage::bignum::BigNumeric;
    let cur = match big.take() {
        Some(b) => *b,
        None => {
            let c = BigNumeric::from_i128(*scaled, *scale);
            *scaled = 0;
            *scale = 0;
            c
        }
    };
    *big = Some(alloc::boxed::Box::new(cur.add(b_in)));
}

/// One sum/avg accumulation step — the same variant arms (and the same
/// error text) as the single-spec fast path's inline match.
#[inline]
/// v7.39 (round 569) — one row's contribution to a min/max lane.
///
/// The same question `accumulate_groups` asks per spec per row, with
/// none of the per-spec indexing around it. NULL contributes nothing,
/// which is PG's rule and the generic path's.
fn fused_extreme_cell(a: &mut FusedAcc, v: &Value<'_>, max: bool) -> Result<(), EvalError> {
    if matches!(v, Value::Null) {
        return Ok(());
    }
    // v7.39 (round 626) — the FOURTH place this comparison is made. The
    // deny list went onto the dispatched arm and the two inlined grouped
    // copies first, and `SELECT min(bool_col) FROM t` — no GROUP BY — still
    // answered, because it lands here.
    if !a.extreme_mysql && min_max_unsupported_type(v) {
        return Err(EvalError::TypeMismatch {
            detail: format!(
                "function {}({}) does not exist",
                if max { "max" } else { "min" },
                crate::conversions::pg_type_name_for_error_opt(v.data_type())
            ),
        });
    }
    let take = match &a.extreme {
        None => true,
        Some(prev) => {
            let ord = extreme_cmp_in(None, a.extreme_coll.as_deref(), v, prev, a.extreme_mysql);
            if max {
                ord == core::cmp::Ordering::Greater
            } else {
                ord == core::cmp::Ordering::Less
            }
        }
    };
    if take {
        a.extreme = Some(v.clone().into_owned());
    }
    Ok(())
}

/// v7.39 (round 626, S05b/F29) — the types PG has no `min`/`max` for.
///
/// A DENY list, not an allow list, and every entry measured: PG accepts
/// min/max over int2 int4 int8 numeric float4 float8 money text varchar
/// bpchar name date time timetz timestamp timestamptz interval bytea inet
/// cidr and the array types, and refuses exactly these. Writing the allow
/// list instead is how round 625's first cut of the string guard managed to
/// refuse five overloads PG actually has; a deny list of measured
/// rejections cannot over-refuse.
fn min_max_unsupported_type(v: &Value<'_>) -> bool {
    matches!(
        v.data_type(),
        Some(
            spg_storage::DataType::Bool
                | spg_storage::DataType::Uuid
                | spg_storage::DataType::Macaddr
                | spg_storage::DataType::Macaddr8
                | spg_storage::DataType::Json
                | spg_storage::DataType::Jsonb
                | spg_storage::DataType::Bit(_)
                | spg_storage::DataType::BitVarying(_)
                | spg_storage::DataType::Xml
                | spg_storage::DataType::TsVector
                | spg_storage::DataType::TsQuery
                // v7.39 (round 641) — a transaction id has no ordering
                // operator, so PG has no `min(xid)` / `max(xid)` either:
                // "function min(xid) does not exist", measured. SPG
                // answered, because a Value::Xid carries a u32 that
                // compares perfectly well — which is exactly the trap
                // the type exists to avoid.
                | spg_storage::DataType::Xid
        )
    )
}

/// Fold one value into a running sum/avg. THE accumulator — there is no
/// second copy, by design; see `NumAcc` for what four copies cost.
///
/// No `inline(always)` here, and the reason is measured rather than
/// stylistic. The four copies were hand-inlining, so the obvious guess was
/// that the collapse would cost a call per row and the attribute would buy
/// it back. It did not: with `count` split out of `NumAcc`, `sum(int)`
/// over 500k rows lost ~8% WITH the attribute applied. What actually
/// mattered was the pointer count — the copy this replaces took one
/// `&mut FusedAcc`, and passing `&mut NumAcc` plus a separate `&mut i64`
/// made two base pointers. Folding `count` back into the struct closed the
/// gap; the attribute never did, so it is not here.
fn acc_cell(a: &mut NumAcc, v: &Value<'_>) -> Result<(), EvalError> {
    match v {
        Value::Null => {}
        Value::SmallInt(n) => {
            a.sum_int += i64::from(*n);
            a.count += 1;
        }
        Value::Int(n) => {
            a.sum_int += i64::from(*n);
            a.count += 1;
        }
        // v7.38 (read01, T4) — BIGINT sums as exact NUMERIC (PG).
        Value::BigInt(n) => {
            sum_add_exact(
                &mut a.sum_num_scaled,
                &mut a.sum_num_scale,
                &mut a.sum_big,
                i128::from(*n),
                0,
            );
            a.use_numeric = true;
            a.count += 1;
        }
        Value::Float(x) => {
            a.sum_float += *x;
            a.use_float = true;
            a.float_not_real = true;
            a.count += 1;
        }
        Value::Real(x) => {
            a.sum_float += f64::from(*x);
            a.use_float = true;
            a.count += 1;
        }
        Value::Numeric {
            scaled,
            scale,
            kind,
        } => {
            sum_add_exact(
                &mut a.sum_num_scaled,
                &mut a.sum_num_scale,
                &mut a.sum_big,
                *scaled,
                *scale,
            );
            a.sum_num_kind = fold_sum_kind(a.sum_num_kind, *kind);
            a.use_numeric = true;
            a.count += 1;
        }
        // v7.39 (read01 numeric.c) — a NumericBig input promotes to the spill.
        Value::NumericBig(b) => {
            sum_add_bignum(
                &mut a.sum_num_scaled,
                &mut a.sum_num_scale,
                &mut a.sum_big,
                b,
            );
            a.use_numeric = true;
            a.count += 1;
        }
        Value::Interval {
            months,
            days,
            micros,
            kind,
        } => {
            a.sum_iv_months += i64::from(*months);
            a.sum_iv_days += i64::from(*days);
            a.sum_iv_micros += i128::from(*micros);
            a.use_interval = true;
            a.count += 1;
        }
        Value::Money(c) => {
            a.sum_money += i128::from(*c);
            a.use_money = true;
            a.count += 1;
        }
        other => {
            return Err(EvalError::TypeMismatch {
                detail: format!(
                    "sum/avg need numeric, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
                ),
            });
        }
    }
    Ok(())
}

/// v7.39 (read01 round 61) — thread the catalog into a stage's context when the
/// caller has one. `EvalContext::with_catalog` takes a reference, so this keeps
/// the Option handling in one place rather than at four call sites.
fn with_catalog<'a>(
    ctx: EvalContext<'a>,
    catalog: Option<&'a spg_storage::Catalog>,
    engine: Option<&'a crate::Engine>,
) -> EvalContext<'a> {
    let ctx = match catalog {
        Some(c) => ctx.with_catalog(c),
        None => ctx,
    };
    match engine {
        Some(e) => ctx.with_engine(e),
        None => ctx,
    }
}

fn accumulate_groups(
    rows: AggRows<'_>,
    group_exprs: &[Expr],
    agg_specs: &[AggSpec],
    schema_cols: &[ColumnSchema],
    table_alias: Option<&str>,
    correlated_eval: Option<CorrelatedEval<'_>>,
    runner: Option<&dyn crate::ParallelRunner>,
    // v7.39 (read01 round 61) — the catalog. `run` has carried it since the
    // enum-order knife, but the four stages below each built a BARE context and
    // dropped it — so a catalog-dependent expression inside an aggregate's
    // argument (`string_agg(f1(id), ',')`, a user function) answered "unknown
    // function". Same family as rounds 49/53/54/55/56.
    catalog: Option<&spg_storage::Catalog>,
    engine: Option<&crate::Engine>,
) -> Result<Vec<(Vec<Value<'static>>, Vec<AggState>)>, EvalError> {
    let ctx = with_catalog(EvalContext::new(schema_cols, table_alias), catalog, engine);
    // Map group key (vec of values, encoded as canonical string) -> group state.
    // v7.32 (architecture v2, P2b) — insertion-ordered group state in
    // a Vec; the hash map only maps key → index. Removes the parallel
    // `key_order: Vec<String>` (a second per-group key clone) and the
    // per-group re-probe `groups[k]` at finalize (24k hash lookups for
    // the inbox shape). The map owns its key once on vacant insert.
    let mut order: Vec<(Vec<Value<'static>>, Vec<AggState>)> = Vec::new();
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
    // v7.37.16 — raw-i64 group map for the single-INT GROUP BY fast path.
    let mut groups_int: hashbrown::HashMap<i64, usize> = hashbrown::HashMap::new();
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
        // v7.37.16 — bind bare names too, via the compiled-WHERE
        // resolver: `compile_column_pos` mirrors resolve_column's
        // happy layers exactly (composite → prefix/alias gate → bare
        // exact → unique suffix) and returns None on anything that
        // would reach an ambiguity / whole-row / error path, so the
        // eval fallback keeps identical semantics. Previously only
        // qualified refs bound (via the looser find_column_pos), so
        // single-table `GROUP BY g` / `avg(v)` ran the per-row
        // eval_expr tree-walk + Vec + encode_key String alloc — the
        // heavy.rs group_by / filter_agg residual loss vs PG18.
        if let Expr::Column(c) = e {
            eval::compile_column_pos(c, &ctx)
        } else {
            None
        }
    };
    let group_pos: Vec<Option<usize>> = group_exprs.iter().map(col_pos).collect();
    let all_groups_bound = group_pos.iter().all(Option::is_some);
    // v7.37.x — single-col GROUP BY on a TEXT-typed column lets the
    // hot loop key the hash map by the column text directly. Resolved
    // once from the bound position against `schema_cols`.
    // v7.39 (round 364, M4 P2) — the raw-text GROUP BY fast path keys
    // by the column's bytes, which cannot fold; a MySQL session takes
    // the general encoder path (which folds) instead.
    let single_text_group_col: bool = !ctx.mysql_dialect
        && group_pos.len() == 1
        && group_pos[0].is_some_and(|p| {
            schema_cols
                .get(p)
                .is_some_and(|c| matches!(c.ty, spg_storage::DataType::Text))
        });
    // v7.37.16 (heavy.rs group_500k 1.12× loss) — single-col GROUP BY on
    // an INTEGER-typed column keys the map by the raw i64 instead of the
    // canonical-string encode ("I{n}|" write! + String-keyed hash probe
    // was ~25-40 ns of the 42 ns/row 500k GROUP BY budget). Mirrors the
    // single-Text fast path; NULLs share `null_group_idx`; a non-integer
    // cell (coercion edge) falls back to the encoded path.
    let single_int_group_col: bool = group_pos.len() == 1
        && group_pos[0].is_some_and(|p| {
            schema_cols.get(p).is_some_and(|c| {
                matches!(
                    c.ty,
                    spg_storage::DataType::SmallInt
                        | spg_storage::DataType::Int
                        | spg_storage::DataType::BigInt
                )
            })
        });
    let arg_pos: Vec<Option<usize>> = agg_specs
        .iter()
        .map(|spec| spec.arg.as_ref().and_then(|e| col_pos(e)))
        .collect();
    // v7.39 (round 370, M4 P4a) — the MySQL dialect folds GROUP BY /
    // DISTINCT text keys (M4 P2), EXCEPT over a column with an explicit
    // `COLLATE utf8mb4_bin` (stored `Binary`), which de-dups byte-wise.
    // A folding default column stores `CaseInsensitive`, so only an
    // explicit binary column suppresses the fold. Multi-column GROUP BY
    // mixing a binary and a folding column is treated byte-wise as a whole
    // (rare; residual).
    let is_binary_key_col = |p: Option<usize>| -> bool {
        p.and_then(|i| schema_cols.get(i))
            .is_some_and(|c| matches!(c.collation, spg_storage::Collation::Binary))
    };
    // v7.39 (round 371, M4 P4b) — a per-expression `… COLLATE utf8mb4_bin`
    // / `BINARY …` key is byte-wise too, so its GROUP BY / DISTINCT does
    // not fold. The clause lowers to a `binary` cast the parser emits.
    let mysql_fold_groups: bool = ctx.mysql_dialect
        && !group_pos.iter().any(|&p| is_binary_key_col(p))
        && !group_exprs
            .iter()
            .any(|e| crate::eval::is_binary_coerced(e));
    // v7.38.18 — the padding mask, built beside the fold mask off the
    // same argument column so the two cannot come from different places.
    let distinct_pads: Vec<bool> = arg_pos
        .iter()
        .map(|&p| {
            p.and_then(|i| schema_cols.get(i))
                .is_some_and(|c| crate::collate::pads_space(c.collation_name.as_deref()))
        })
        .collect();
    let distinct_fold_case: Vec<bool> = arg_pos.iter().map(|&p| !is_binary_key_col(p)).collect();
    let distinct_fold: Vec<bool> = agg_specs
        .iter()
        .enumerate()
        .map(|(i, spec)| {
            // v7.38.18 — a byte-wise column still needs this step when
            // its collation PADS. `utf8mb4_bin` folds no case and
            // ignores trailing spaces, which is one flag short of what
            // a single boolean can say; the pad mask beside this one
            // carries the second half and the fold is skipped by
            // `distinct_fold_case` below.
            ctx.mysql_dialect
                && (!is_binary_key_col(arg_pos[i]) || distinct_pads[i])
                && !spec
                    .arg
                    .as_ref()
                    .is_some_and(|e| crate::eval::is_binary_coerced(e))
        })
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
    // v7.37.43 (DISTA A-3) — precompute the per-spec arg2 when it is a
    // bare literal. `string_agg(DISTINCT col, ',')` and every other
    // call with a constant separator goes through this path; PG evaluates
    // arg2 as a Const once at plan time. SPG was paying a Cow row
    // materialisation per input row purely so `eval_arg(literal, &row)`
    // could run — but a literal doesn't read the row at all. Hoist the
    // literal value into a per-query table; per-row arg2 just clones it.
    //
    // Sentinel: when arg2 is present but NOT a literal, the entry stays
    // `None` and the per-row path still falls into the eval branch
    // (which forces `needs_mat`).
    let arg2_literal_val: Vec<Option<Value<'static>>> = agg_specs
        .iter()
        .map(|s| match &s.arg2 {
            Some(Expr::Literal(l)) => Some(eval::literal_to_value(l)),
            _ => None,
        })
        .collect();
    // Does any spec need the fully-materialised row in the bound fast
    // path — a FILTER, a non-bound value arg, a NON-LITERAL second arg,
    // or a non-bound ORDER key? When false (every aggregate arg/key is a
    // bound column — the inbox shape, and the DISTA shape after A-3)
    // the bound fast path never materialises a row.
    let needs_mat = agg_specs.iter().enumerate().any(|(i, s)| {
        s.filter.is_some()
            || (s.arg.is_some() && arg_pos[i].is_none() && arg_compiled[i].is_none())
            || (s.arg2.is_some() && arg2_literal_val[i].is_none())
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
    // v7.37.9 T3 S2 — elided lifetime so the Vec's `'val` binds to the
    // row-borrow lifetime per call (`eval_compiled_ref<'row, 'val>` now
    // requires `'row: 'val`). Caller-side Vec<Value<'_>> lets compiler
    // infer the shortest lifetime that covers all calls.
    let mut eval_stack: Vec<Value<'_>> = Vec::new();
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
    let eval_arg =
        |e: &Expr, r: &Row<'static>, c: &EvalContext<'_>| -> Result<Value<'static>, EvalError> {
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
        state.num.count = rows.len() as i64;
        return Ok(order);
    }
    // v7.37.16 (heavy.rs agg_500k 1.6× loss) — fused streaming accumulator
    // for ANY number of count(*)/count(col)/sum(col)/avg(col) specs over
    // BOUND columns (no FILTER/DISTINCT/arg2/ORDER). The generic per-row
    // spec loop paid arg dispatch + union-typed update_state per spec per
    // row (~10 ns/spec/row); PG's parallel agg runs the 500k 3-spec shape
    // at ~18 ns/row effective. Three cuts:
    // - count(*) never enters the row loop — it IS rows.len();
    // - sum/avg over the SAME column share one accumulator (identical
    //   running state), so `count(*), sum(v), avg(v)` does ONE cell read
    //   and one accumulate per row;
    // - remaining ops run in one tight pass, no update_state.
    // Finalize writes the same AggState fields as the single-spec path.
    if single_anon_group
        && let Some((spec_src, unique_ops)) = fused_layout(
            agg_specs,
            &arg_pos,
            &arg_compiled,
            &order_pos,
            &arg2_literal_val,
        )
    {
        let mut accs: Vec<FusedAcc> = fused_accs(&unique_ops, ctx.mysql_dialect);
        // v7.39 (parallel-agg P1) — shard the row scan across the
        // host-injected executor when the input is large enough.
        // Each shard runs the same tight loop over its row range and
        // returns its own Vec<FusedAcc>; the merge is field-wise
        // (see merge_fused). Errors inside a shard surface as the
        // shard result and re-raise after join.
        // v7.39 (round 716) — the scan takes its EvalContext as a
        // parameter: `EvalContext` is not Sync (per-eval memo Cells, the
        // sequence resolver's plain `&dyn Fn`), so the parallel branch
        // hands each shard a locally-built minimal context instead of
        // capturing the outer one. The compiled ops only reach the parts
        // a shard context carries — columns, alias, dialect, catalog —
        // because `fully_compilable` excludes everything else (params,
        // sequences, user functions, FTS).
        let fused_scan = |range: core::ops::Range<usize>,
                          accs: &mut Vec<FusedAcc>,
                          fctx: &EvalContext<'_>|
         -> Result<(), EvalError> {
            // One Step-VM stack per shard call, reused across every
            // row and every compiled op.
            let mut stack: Vec<Value<'_>> = Vec::new();
            for row in rows.range(range.start, range.end).iter() {
                for (si, op) in unique_ops.iter().enumerate() {
                    match op {
                        FusedOp::CountCol(p) => {
                            if !matches!(row.get(*p), Some(Value::Null) | None) {
                                accs[si].num.count += 1;
                            }
                        }
                        FusedOp::AccCol(p) => {
                            {
                                let a = &mut accs[si];
                                acc_cell(&mut a.num, row.get(*p).unwrap_or(&Value::Null))
                            }?;
                        }
                        FusedOp::Extreme { pos, max, .. } => {
                            fused_extreme_cell(
                                &mut accs[si],
                                row.get(*pos).unwrap_or(&Value::Null),
                                *max,
                            )?;
                        }
                        FusedOp::CountExpr(sp) => {
                            let c = arg_compiled[*sp].as_ref().expect("gated compiled");
                            let v = eval::eval_compiled_ref(c, row, fctx, &mut stack)?;
                            if !matches!(v, Value::Null) {
                                accs[si].num.count += 1;
                            }
                        }
                        FusedOp::AccExpr(sp) => {
                            let c = arg_compiled[*sp].as_ref().expect("gated compiled");
                            let v = eval::eval_compiled_ref(c, row, fctx, &mut stack)?;
                            acc_cell(&mut accs[si].num, &v)?;
                        }
                        FusedOp::ExtremeExpr { spec, max, .. } => {
                            let c = arg_compiled[*spec].as_ref().expect("gated compiled");
                            let v = eval::eval_compiled_ref(c, row, fctx, &mut stack)?;
                            fused_extreme_cell(&mut accs[si], &v, *max)?;
                        }
                        FusedOp::Collect { spec, string_kind } => {
                            collect_cell(
                                &mut accs[si],
                                &row,
                                arg_pos[*spec].expect("gated bound"),
                                &order_pos[*spec],
                                *string_kind,
                            )?;
                        }
                    }
                }
            }
            Ok(())
        };
        if !unique_ops.is_empty() {
            let par = runner.filter(|_| rows.len() >= crate::PARALLEL_MIN_ROWS);
            if let Some(r) = par {
                crate::PARALLEL_AGG_FIRED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                let n_shards = (rows.len() / crate::PARALLEL_MIN_ROWS).clamp(2, 8);
                let chunk = rows.len().div_ceil(n_shards);
                type ShardOut = Result<Vec<FusedAcc>, EvalError>;
                let ops = &unique_ops;
                let mysql_for_accs = ctx.mysql_dialect;
                // v7.39 (round 716) — the whitelisted concat family
                // renders through the SESSION's style; a shard context
                // built from defaults would silently re-render dates and
                // floats the default way. RenderStyle is Copy.
                let outer_style = ctx.render_style;
                let results = r.run_shards(n_shards, &|i| {
                    let lo = i * chunk;
                    let hi = ((i + 1) * chunk).min(rows.len());
                    let mut local: Vec<FusedAcc> = fused_accs(ops, mysql_for_accs);
                    // Shard-local minimal context (the outer one is not
                    // Sync); see the fused_scan comment.
                    let mut sctx = EvalContext::new(schema_cols, table_alias);
                    sctx.mysql_dialect = mysql_for_accs;
                    sctx.render_style = outer_style;
                    let sctx = match catalog {
                        Some(c) => sctx.with_catalog(c),
                        None => sctx,
                    };
                    let out: ShardOut = fused_scan(lo..hi, &mut local, &sctx).map(|()| local);
                    alloc::boxed::Box::new(out)
                });
                for boxed in results {
                    let shard = boxed
                        .downcast::<ShardOut>()
                        .expect("runner echoes the closure's box");
                    let mut shard_accs = (*shard)?;
                    for (si, b) in shard_accs.iter_mut().enumerate() {
                        merge_fused(&mut accs[si], b);
                    }
                }
            } else {
                fused_scan(0..rows.len(), &mut accs, &ctx)?;
            }
        }
        fill_states_from_fused(
            &mut order[0].1,
            &spec_src,
            &mut accs,
            rows.len() as i64,
            &arg2_literal_val,
        );
        return Ok(order);
    }
    // v7.39 (parallel-agg P3) — parallel GROUP BY fast path: a single
    // bound INT group column with every spec fused-eligible (the
    // `GROUP BY g` + count/sum/avg panel shape). Shards build local
    // i64-keyed maps of FusedAcc slots; the merge folds maps in shard
    // order (first-seen group order across shards — SQL leaves GROUP
    // BY output order unspecified). Any non-integer cell under the
    // integer schema (coercion edge) aborts the shard and the whole
    // scan falls back to the serial path below.
    if single_int_group_col
        && group_exprs.len() == 1
        && rows.len() >= crate::PARALLEL_MIN_ROWS
        && let Some(r) = runner
        && let Some((spec_src, unique_ops)) = fused_layout(
            agg_specs,
            &arg_pos,
            &arg_compiled,
            &order_pos,
            &arg2_literal_val,
        )
        && !unique_ops.is_empty()
    {
        crate::PARALLEL_AGG_FIRED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let gp = group_pos[0].expect("single_int_group_col implies bound");
        struct ShardMap {
            // first-seen order of keys within the shard.
            keys: Vec<(i64, Value<'static>)>,
            slots: hashbrown::HashMap<i64, Vec<FusedAcc>>,
            null_slot: Option<Vec<FusedAcc>>,
            null_rows: i64,
            key_rows: hashbrown::HashMap<i64, i64>,
        }
        // Err(None) = coercion edge -> serial fallback; Err(Some(e)) = real error.
        type ShardOut = Result<ShardMap, Option<EvalError>>;
        let n_shards = (rows.len() / crate::PARALLEL_MIN_ROWS).clamp(2, 8);
        let chunk = rows.len().div_ceil(n_shards);
        let ops = &unique_ops;
        let mysql_for_accs = ctx.mysql_dialect;
        // Same session-style carry as the anonymous-group lane.
        let outer_style = ctx.render_style;
        let results = r.run_shards(n_shards, &|si| {
            let lo = si * chunk;
            let hi = ((si + 1) * chunk).min(rows.len());
            let mut m = ShardMap {
                keys: Vec::new(),
                slots: hashbrown::HashMap::new(),
                null_slot: None,
                null_rows: 0,
                key_rows: hashbrown::HashMap::new(),
            };
            let out: ShardOut = (|| {
                // v7.39 (round 716) — per-shard Step-VM stack for the
                // compiled-argument ops, reused across rows, plus a
                // shard-local minimal context (the outer one is not
                // Sync); see the anonymous-group fused_scan comment.
                let mut stack: Vec<Value<'_>> = Vec::new();
                let mut sctx = EvalContext::new(schema_cols, table_alias);
                sctx.mysql_dialect = mysql_for_accs;
                sctx.render_style = outer_style;
                let sctx = match catalog {
                    Some(c) => sctx.with_catalog(c),
                    None => sctx,
                };
                for row in rows.range(lo, hi).iter() {
                    let v = row.get(gp).unwrap_or(&Value::Null);
                    let key: Option<i64> = match v {
                        Value::SmallInt(n) => Some(i64::from(*n)),
                        Value::Int(n) => Some(i64::from(*n)),
                        Value::BigInt(n) => Some(*n),
                        Value::Null => None,
                        _ => return Err(None), // coercion edge -> serial
                    };
                    let slots = match key {
                        Some(k) => {
                            *m.key_rows.entry(k).or_insert(0) += 1;
                            m.slots.entry(k).or_insert_with(|| {
                                m.keys.push((k, v.clone().into_owned()));
                                fused_accs(ops, mysql_for_accs)
                            })
                        }
                        None => {
                            m.null_rows += 1;
                            m.null_slot
                                .get_or_insert_with(|| fused_accs(ops, mysql_for_accs))
                        }
                    };
                    for (oi, op) in ops.iter().enumerate() {
                        match op {
                            FusedOp::CountCol(p) => {
                                if !matches!(row.get(*p), Some(Value::Null) | None) {
                                    slots[oi].num.count += 1;
                                }
                            }
                            FusedOp::AccCol(p) => {
                                {
                                    let a = &mut slots[oi];
                                    acc_cell(&mut a.num, row.get(*p).unwrap_or(&Value::Null))
                                }
                                .map_err(Some)?;
                            }
                            FusedOp::Extreme { pos, max, .. } => {
                                fused_extreme_cell(
                                    &mut slots[oi],
                                    row.get(*pos).unwrap_or(&Value::Null),
                                    *max,
                                )
                                .map_err(Some)?;
                            }
                            FusedOp::CountExpr(sp) => {
                                let c = arg_compiled[*sp].as_ref().expect("gated compiled");
                                let v = eval::eval_compiled_ref(c, row, &sctx, &mut stack)
                                    .map_err(Some)?;
                                if !matches!(v, Value::Null) {
                                    slots[oi].num.count += 1;
                                }
                            }
                            FusedOp::AccExpr(sp) => {
                                let c = arg_compiled[*sp].as_ref().expect("gated compiled");
                                let v = eval::eval_compiled_ref(c, row, &sctx, &mut stack)
                                    .map_err(Some)?;
                                acc_cell(&mut slots[oi].num, &v).map_err(Some)?;
                            }
                            FusedOp::ExtremeExpr { spec, max, .. } => {
                                let c = arg_compiled[*spec].as_ref().expect("gated compiled");
                                let v = eval::eval_compiled_ref(c, row, &sctx, &mut stack)
                                    .map_err(Some)?;
                                fused_extreme_cell(&mut slots[oi], &v, *max).map_err(Some)?;
                            }
                            FusedOp::Collect { spec, string_kind } => {
                                collect_cell(
                                    &mut slots[oi],
                                    &row,
                                    arg_pos[*spec].expect("gated bound"),
                                    &order_pos[*spec],
                                    *string_kind,
                                )
                                .map_err(Some)?;
                            }
                        }
                    }
                }
                Ok(m)
            })();
            alloc::boxed::Box::new(out)
        });
        // Merge in shard order; a fallback sentinel drops to serial.
        let mut merged_keys: Vec<(i64, Value<'static>)> = Vec::new();
        let mut merged: hashbrown::HashMap<i64, (Vec<FusedAcc>, i64)> = hashbrown::HashMap::new();
        let mut merged_null: Option<(Vec<FusedAcc>, i64)> = None;
        let mut fallback = false;
        let mut shard_err: Option<EvalError> = None;
        for boxed in results {
            let shard = boxed
                .downcast::<ShardOut>()
                .expect("runner echoes the closure's box");
            match *shard {
                Ok(mut m) => {
                    for (k, kv) in m.keys {
                        // Removed (not borrowed): the slot MOVES into the
                        // merged map on first sight, and the round-724
                        // collection lanes move out of it on merge.
                        let mut accs = m.slots.remove(&k).expect("keyed slot");
                        let rows_k = m.key_rows[&k];
                        match merged.get_mut(&k) {
                            Some((dst, cnt)) => {
                                for (i, b) in accs.iter_mut().enumerate() {
                                    merge_fused(&mut dst[i], b);
                                }
                                *cnt += rows_k;
                            }
                            None => {
                                merged_keys.push((k, kv));
                                merged.insert(k, (accs, rows_k));
                            }
                        }
                    }
                    if let Some(mut nb) = m.null_slot.take() {
                        match &mut merged_null {
                            Some((dst, cnt)) => {
                                for (i, b) in nb.iter_mut().enumerate() {
                                    merge_fused(&mut dst[i], b);
                                }
                                *cnt += m.null_rows;
                            }
                            None => merged_null = Some((nb, m.null_rows)),
                        }
                    }
                }
                Err(None) => fallback = true,
                Err(Some(e)) => shard_err = Some(e),
            }
        }
        if let Some(e) = shard_err {
            return Err(e);
        }
        if !fallback {
            for (k, kv) in merged_keys {
                let (mut accs, group_rows) = merged.remove(&k).expect("key recorded");
                let mut states: Vec<AggState> =
                    (0..agg_specs.len()).map(|_| AggState::default()).collect();
                fill_states_from_fused(
                    &mut states,
                    &spec_src,
                    &mut accs,
                    group_rows,
                    &arg2_literal_val,
                );
                order.push((alloc::vec![kv], states));
            }
            if let Some((mut accs, group_rows)) = merged_null {
                let mut states: Vec<AggState> =
                    (0..agg_specs.len()).map(|_| AggState::default()).collect();
                fill_states_from_fused(
                    &mut states,
                    &spec_src,
                    &mut accs,
                    group_rows,
                    &arg2_literal_val,
                );
                order.push((alloc::vec![Value::Null], states));
            }
            return Ok(order);
        }
        // fallthrough: serial paths below handle the coercion edge.
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
        for row in rows.iter() {
            if !matches!(row.get(p), Some(Value::Null) | None) {
                count += 1;
            }
        }
        let state = &mut order[0].1[0];
        state.num.count = count;
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
        // v7.39 (round 665) — was fifteen loose locals mirroring
        // `NumAcc` field for field; `FusedAcc`'s doc comment even
        // said so. One struct now, folded by the one `acc_cell`.
        let mut na = NumAcc::default();
        // Borrow-aware fast inner: avoid the per-row clone when arg
        // is a bound column position.
        if let Some(p) = arg_pos0 {
            for row in rows.iter() {
                let v_ref = row.get(p).unwrap_or(&Value::Null);
                acc_cell(&mut na, v_ref)?;
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
            for row in rows.iter() {
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
                            detail: format!(
                                "length() needs text, got {}",
                                crate::conversions::pg_type_name_for_error_opt(other.data_type())
                            ),
                        });
                    }
                };
                na.sum_int += n;
                na.count += 1;
            }
        } else {
            let c = arg_c0.as_ref().unwrap();
            for row in rows.iter() {
                let v = eval::eval_compiled_ref(c, row, &ctx, &mut eval_stack)?;
                acc_cell(&mut na, &v)?;
            }
        }
        let state = &mut order[0].1[0];
        state.num = na;
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
        for row in rows.iter() {
            let kv = row.get(gpos).unwrap_or(&Value::Null);
            let idx = match kv {
                Value::Text(s) => match groups_text.get(s.as_ref()) {
                    Some(&i) => i,
                    None => {
                        let i = order.len();
                        order.push((
                            alloc::vec![Value::text(s.clone())],
                            alloc::vec![AggState::default()],
                        ));
                        groups_text.insert(s.to_string(), i);
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
                    encode_key_refs_into_in(&refs, &mut keybuf_s, mysql_fold_groups);
                    match groups.get(keybuf_s.as_str()) {
                        Some(&i) => i,
                        None => {
                            let i = order.len();
                            order.push((
                                alloc::vec![kv.clone().into_owned()],
                                alloc::vec![AggState::default()],
                            ));
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
                    Some(prev) => {
                        extreme_cmp_in(
                            agg_specs[0].enum_labels.as_deref(),
                            agg_specs[0].arg_collation.as_deref(),
                            av,
                            prev,
                            ctx.mysql_dialect,
                        ) == core::cmp::Ordering::Greater
                    }
                };
                if upd {
                    st.extreme = Some(av.clone().into_owned());
                }
            }
        }
        return Ok(order);
    }

    for row in rows.iter() {
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
                    (Some(p), _, _) => {
                        // v7.37.9 Phase 1A-ext counter — fast position-bound arg.
                        crate::bump_counter!(AGG_PER_ROW_FAST_POS);
                        row.get(*p).unwrap_or(&Value::Null)
                    }
                    (None, None, None) => {
                        // COUNT(*) sentinel
                        crate::bump_counter!(AGG_PER_ROW_COUNT_STAR_SENTINEL);
                        arg_owned = Value::Bool(true);
                        &arg_owned
                    }
                    (None, Some(s), _) => {
                        if row_eval_cache[s].is_none() {
                            // v7.37.9 Phase 1A-ext counter — Step-VM ran (cache miss).
                            crate::bump_counter!(AGG_PER_ROW_COMPILED_MISS);
                            let c = arg_compiled[arg_unique_idx[s]]
                                .as_ref()
                                .expect("arg_unique_idx points at a compiled spec");
                            let v = eval::eval_compiled_ref(c, row, &ctx, &mut eval_stack)?;
                            row_eval_cache[s] = Some(v);
                        } else {
                            // v7.37.9 Phase 1A-ext counter — CSE cache hit
                            // (compiled arg deduped across specs in same row).
                            crate::bump_counter!(AGG_PER_ROW_COMPILED_HIT);
                        }
                        row_eval_cache[s].as_ref().expect("just filled above")
                    }
                    (None, None, Some(e)) => {
                        // v7.37.9 Phase 1A-ext counter — eval_expr fallback
                        // (uncompilable spec — Cow row materialise per row).
                        crate::bump_counter!(AGG_PER_ROW_EVAL_FALLBACK);
                        arg_owned = eval_arg(
                            e,
                            mat.as_deref().expect("needs_mat for non-bound arg"),
                            &ctx,
                        )?;
                        &arg_owned
                    }
                };
                let arg2_val = match (&spec.arg2, &arg2_literal_val[i]) {
                    (None, _) => None,
                    // v7.37.43 (DISTA A-3) — literal arg2: clone the
                    // precomputed value, skip per-row eval & row mat.
                    (Some(_), Some(lit)) => {
                        // v7.37.9 Phase 0 diagnostic — count per-row
                        // hits of the DISTA A-3 fast path.
                        crate::bump_counter!(DISTA_LITERAL_ARG2_CACHE_FIRE);
                        Some(lit.clone())
                    }
                    (Some(e), None) => Some(eval_arg(
                        e,
                        mat.as_deref().expect("needs_mat for arg2"),
                        &ctx,
                    )?),
                };
                let order_keys: Option<Vec<Value<'static>>> = if spec.order_by.is_empty() {
                    None
                } else {
                    crate::bump_counter!(AGGREGATE_ARRAY_AGG_ORDER_BY_FIRE);
                    let mut keys: Vec<Value<'static>> = Vec::with_capacity(spec.order_by.len());
                    for (k, o) in spec.order_by.iter().enumerate() {
                        let v: Value<'static> = if let Some(p) = order_pos[i][k] {
                            row.get(p)
                                .cloned()
                                .map(Value::into_owned)
                                .unwrap_or(Value::Null)
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
                                cmp_order_keys(
                                    &spec.order_by,
                                    &spec.order_enum_labels,
                                    &spec.order_collations,
                                    &keys,
                                    bk,
                                    ctx.mysql_dialect,
                                ) == core::cmp::Ordering::Less
                            }
                        };
                        if better {
                            st.first_best = Some((keys, arg_ref.clone().into_owned()));
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
                    //
                    // v7.37.x (docker-fair DISTA attack) — extend the
                    // single-family fast path to BigInt via a parallel
                    // `seen_int: Option<BTreeSet<i64>>`. The DISTA
                    // `COUNT(DISTINCT m.id)` shape pumps 25 k BigInt
                    // probes; skipping `encode_key_refs_into` saves
                    // ~100 ns of alloc + format churn per row.
                    if let Value::Text(s) = arg_ref {
                        // v7.39 (round 364, M4 P2) — a MySQL session folds
                        // the distinct key (case/accent) so `Foo`/`foo`
                        // count once. The `seen` set stays internally
                        // consistent: both probe and insert fold.
                        // v7.39 (round 370, M4 P4a) — but an explicit
                        // `COLLATE utf8mb4_bin` column de-dups byte-wise.
                        if distinct_fold[i] {
                            // v7.38.18 — and pad when the argument's
                            // collation says trailing spaces do not
                            // count. `utf8mb4_general_ci` folds AND
                            // pads; `utf8mb4_0900_ai_ci` only folds.
                            let base = if distinct_pads[i] {
                                s.trim_end_matches(' ')
                            } else {
                                s.as_ref()
                            };
                            let k = if distinct_fold_case[i] {
                                spg_storage::mysql_ci_fold(base)
                            } else {
                                alloc::string::ToString::to_string(base)
                            };
                            if entry.1[i].seen.contains(k.as_str()) {
                                continue;
                            }
                            entry.1[i].seen.insert(k);
                        } else {
                            if entry.1[i].seen.contains(s.as_ref()) {
                                continue;
                            }
                            entry.1[i].seen.insert(s.to_string());
                        }
                    } else if let Value::BigInt(n) = arg_ref {
                        let set = entry.1[i].seen_int.get_or_insert_with(BTreeSet::new);
                        if !set.insert(*n) {
                            continue;
                        }
                    } else if let Value::Int(n) = arg_ref {
                        let set = entry.1[i].seen_int.get_or_insert_with(BTreeSet::new);
                        if !set.insert(i64::from(*n)) {
                            continue;
                        }
                    } else {
                        encode_key_refs_into_in(
                            core::slice::from_ref(&arg_ref),
                            &mut dkeybuf,
                            distinct_fold[i],
                        );
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
                            // v7.39 (round 626) — the same deny list the
                            // dispatched path applies. These inlined copies
                            // exist for speed and are where `min(TRUE)`
                            // actually lands, so a guard placed only on the
                            // dispatched arm never fires.
                            if !ctx.mysql_dialect && min_max_unsupported_type(arg_ref) {
                                return Err(EvalError::TypeMismatch {
                                    detail: format!(
                                        "function max({}) does not exist",
                                        crate::conversions::pg_type_name_for_error_opt(
                                            arg_ref.data_type()
                                        )
                                    ),
                                });
                            }
                            let st = &mut entry.1[i];
                            let upd = match &st.extreme {
                                None => true,
                                Some(prev) => {
                                    extreme_cmp_in(
                                        spec.enum_labels.as_deref(),
                                        spec.arg_collation.as_deref(),
                                        arg_ref,
                                        prev,
                                        ctx.mysql_dialect,
                                    ) == core::cmp::Ordering::Greater
                                }
                            };
                            if upd {
                                st.extreme = Some(arg_ref.clone().into_owned());
                            }
                        }
                    }
                    AggKind::Min => {
                        if !matches!(arg_ref, Value::Null) {
                            // v7.39 (round 626) — see the Max arm above.
                            if !ctx.mysql_dialect && min_max_unsupported_type(arg_ref) {
                                return Err(EvalError::TypeMismatch {
                                    detail: format!(
                                        "function min({}) does not exist",
                                        crate::conversions::pg_type_name_for_error_opt(
                                            arg_ref.data_type()
                                        )
                                    ),
                                });
                            }
                            let st = &mut entry.1[i];
                            let upd = match &st.extreme {
                                None => true,
                                Some(prev) => {
                                    extreme_cmp_in(
                                        spec.enum_labels.as_deref(),
                                        spec.arg_collation.as_deref(),
                                        arg_ref,
                                        prev,
                                        ctx.mysql_dialect,
                                    ) == core::cmp::Ordering::Less
                                }
                            };
                            if upd {
                                st.extreme = Some(arg_ref.clone().into_owned());
                            }
                        }
                    }
                    AggKind::AnyValue => {
                        if !matches!(arg_ref, Value::Null) {
                            let st = &mut entry.1[i];
                            if st.extreme.is_none() {
                                st.extreme = Some(arg_ref.clone().into_owned());
                            }
                        }
                    }
                    AggKind::CountStar => {
                        entry.1[i].num.count += 1;
                    }
                    AggKind::Count => {
                        if !matches!(arg_ref, Value::Null) {
                            entry.1[i].num.count += 1;
                        }
                    }
                    AggKind::BoolOr => match arg_ref {
                        Value::Bool(b) => {
                            let st = &mut entry.1[i];
                            st.bool_acc = Some(st.bool_acc.unwrap_or(false) || *b);
                        }
                        Value::Null => {}
                        _ => update_state(
                            &mut entry.1[i],
                            spec.kind,
                            &spec.name,
                            arg_ref,
                            arg2_val.as_ref(),
                            order_keys,
                            spec.enum_labels.as_deref(),
                            spec.arg_collation.as_deref(),
                            ctx.mysql_dialect,
                        )?,
                    },
                    AggKind::BoolAnd => match arg_ref {
                        Value::Bool(b) => {
                            let st = &mut entry.1[i];
                            st.bool_acc = Some(st.bool_acc.unwrap_or(true) && *b);
                        }
                        Value::Null => {}
                        _ => update_state(
                            &mut entry.1[i],
                            spec.kind,
                            &spec.name,
                            arg_ref,
                            arg2_val.as_ref(),
                            order_keys,
                            spec.enum_labels.as_deref(),
                            spec.arg_collation.as_deref(),
                            ctx.mysql_dialect,
                        )?,
                    },
                    _ => {
                        update_state(
                            &mut entry.1[i],
                            spec.kind,
                            &spec.name,
                            arg_ref,
                            arg2_val.as_ref(),
                            order_keys,
                            spec.enum_labels.as_deref(),
                            spec.arg_collation.as_deref(),
                            ctx.mysql_dialect,
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
                    Value::Text(s) => match groups_text.get(s.as_ref()) {
                        Some(&i) => i,
                        None => {
                            let i = order.len();
                            let init: Vec<AggState> =
                                (0..agg_specs.len()).map(|_| AggState::default()).collect();
                            order.push((alloc::vec![Value::text(s.clone())], init));
                            groups_text.insert(s.to_string(), i);
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
                        encode_key_refs_into_in(&refs, &mut keybuf_s, mysql_fold_groups);
                        match groups.get(keybuf_s.as_str()) {
                            Some(&i) => i,
                            None => {
                                let i = order.len();
                                let init: Vec<AggState> =
                                    (0..agg_specs.len()).map(|_| AggState::default()).collect();
                                order.push((alloc::vec![v.clone().into_owned()], init));
                                groups.insert(keybuf_s.clone(), i);
                                i
                            }
                        }
                    }
                }
            } else if single_int_group_col {
                // v7.37.16 — raw-i64 keying (see single_int_group_col).
                let v = row.get(group_pos[0].unwrap()).unwrap_or(&Value::Null);
                let key: Option<i64> = match v {
                    Value::SmallInt(n) => Some(i64::from(*n)),
                    Value::Int(n) => Some(i64::from(*n)),
                    Value::BigInt(n) => Some(*n),
                    _ => None,
                };
                match (key, v) {
                    (Some(k), _) => match groups_int.get(&k) {
                        Some(&i) => i,
                        None => {
                            let i = order.len();
                            let init: Vec<AggState> =
                                (0..agg_specs.len()).map(|_| AggState::default()).collect();
                            order.push((alloc::vec![v.clone().into_owned()], init));
                            groups_int.insert(k, i);
                            i
                        }
                    },
                    (None, Value::Null) => match null_group_idx {
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
                    (None, _) => {
                        // Non-integer cell under an integer schema
                        // (coercion edge) — encoded-path fallback.
                        refs.clear();
                        refs.push(v);
                        encode_key_refs_into_in(&refs, &mut keybuf_s, mysql_fold_groups);
                        match groups.get(keybuf_s.as_str()) {
                            Some(&i) => i,
                            None => {
                                let i = order.len();
                                let init: Vec<AggState> =
                                    (0..agg_specs.len()).map(|_| AggState::default()).collect();
                                order.push((alloc::vec![v.clone().into_owned()], init));
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
                encode_key_refs_into_in(&refs, &mut keybuf_s, mysql_fold_groups);
                match groups.get(keybuf_s.as_str()) {
                    Some(&i) => i,
                    None => {
                        let i = order.len();
                        let init: Vec<AggState> =
                            (0..agg_specs.len()).map(|_| AggState::default()).collect();
                        let owned: Vec<Value<'static>> =
                            refs.iter().map(|v| (*v).clone().into_owned()).collect();
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
                    (Some(p), _, _) => {
                        crate::bump_counter!(AGG_PER_ROW_FAST_POS);
                        row.get(*p).unwrap_or(&Value::Null)
                    }
                    (None, None, None) => {
                        crate::bump_counter!(AGG_PER_ROW_COUNT_STAR_SENTINEL);
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
                            crate::bump_counter!(AGG_PER_ROW_COMPILED_MISS);
                            let c = arg_compiled[arg_unique_idx[s]]
                                .as_ref()
                                .expect("arg_unique_idx points at a compiled spec");
                            let v = eval::eval_compiled_ref(c, row, &ctx, &mut eval_stack)?;
                            row_eval_cache[s] = Some(v);
                        } else {
                            crate::bump_counter!(AGG_PER_ROW_COMPILED_HIT);
                        }
                        row_eval_cache[s].as_ref().expect("just filled above")
                    }
                    (None, None, Some(e)) => {
                        crate::bump_counter!(AGG_PER_ROW_EVAL_FALLBACK);
                        arg_owned = eval_arg(
                            e,
                            mat.as_deref().expect("needs_mat for non-bound arg"),
                            &ctx,
                        )?;
                        &arg_owned
                    }
                };
                let arg2_val = match (&spec.arg2, &arg2_literal_val[i]) {
                    (None, _) => None,
                    // v7.37.43 (DISTA A-3) — literal arg2: clone the
                    // precomputed value, skip per-row eval & row mat.
                    (Some(_), Some(lit)) => {
                        // v7.37.9 Phase 0 diagnostic — count per-row
                        // hits of the DISTA A-3 fast path.
                        crate::bump_counter!(DISTA_LITERAL_ARG2_CACHE_FIRE);
                        Some(lit.clone())
                    }
                    (Some(e), None) => Some(eval_arg(
                        e,
                        mat.as_deref().expect("needs_mat for arg2"),
                        &ctx,
                    )?),
                };
                let order_keys: Option<Vec<Value<'static>>> = if spec.order_by.is_empty() {
                    None
                } else {
                    crate::bump_counter!(AGGREGATE_ARRAY_AGG_ORDER_BY_FIRE);
                    let mut keys: Vec<Value<'static>> = Vec::with_capacity(spec.order_by.len());
                    for (k, o) in spec.order_by.iter().enumerate() {
                        // Bound ORDER key → read the cell by reference; only
                        // a non-bound key falls to the materialised eval path.
                        keys.push(match order_pos[i][k] {
                            Some(p) => row
                                .get(p)
                                .cloned()
                                .map(Value::into_owned)
                                .unwrap_or(Value::Null),
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
                                cmp_order_keys(
                                    &spec.order_by,
                                    &spec.order_enum_labels,
                                    &spec.order_collations,
                                    &keys,
                                    bk,
                                    ctx.mysql_dialect,
                                ) == core::cmp::Ordering::Less
                            }
                        };
                        if better {
                            st.first_best = Some((keys, arg_ref.clone().into_owned()));
                        }
                    }
                    continue;
                }
                if spec.distinct {
                    // v7.37.x — single-Text DISTINCT fast path (see
                    // bound fast path counterpart above). Per-spec
                    // type invariance lets us use the column text as
                    // the `seen` key directly, no `S<text>|` prefix.
                    // v7.37.x (docker-fair DISTA) — BigInt parallel
                    // path skips encode_key_refs_into entirely.
                    if let Value::Text(s) = arg_ref {
                        if entry.1[i].seen.contains(s.as_ref()) {
                            continue;
                        }
                        entry.1[i].seen.insert(s.to_string());
                    } else if let Value::BigInt(n) = arg_ref {
                        let set = entry.1[i].seen_int.get_or_insert_with(BTreeSet::new);
                        if !set.insert(*n) {
                            continue;
                        }
                    } else if let Value::Int(n) = arg_ref {
                        let set = entry.1[i].seen_int.get_or_insert_with(BTreeSet::new);
                        if !set.insert(i64::from(*n)) {
                            continue;
                        }
                    } else {
                        encode_key_refs_into_in(
                            core::slice::from_ref(&arg_ref),
                            &mut dkeybuf,
                            distinct_fold[i],
                        );
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
                            // v7.39 (round 626) — the same deny list the
                            // dispatched path applies. These inlined copies
                            // exist for speed and are where `min(TRUE)`
                            // actually lands, so a guard placed only on the
                            // dispatched arm never fires.
                            if !ctx.mysql_dialect && min_max_unsupported_type(arg_ref) {
                                return Err(EvalError::TypeMismatch {
                                    detail: format!(
                                        "function max({}) does not exist",
                                        crate::conversions::pg_type_name_for_error_opt(
                                            arg_ref.data_type()
                                        )
                                    ),
                                });
                            }
                            let st = &mut entry.1[i];
                            let upd = match &st.extreme {
                                None => true,
                                Some(prev) => {
                                    extreme_cmp_in(
                                        spec.enum_labels.as_deref(),
                                        spec.arg_collation.as_deref(),
                                        arg_ref,
                                        prev,
                                        ctx.mysql_dialect,
                                    ) == core::cmp::Ordering::Greater
                                }
                            };
                            if upd {
                                st.extreme = Some(arg_ref.clone().into_owned());
                            }
                        }
                    }
                    AggKind::Min => {
                        if !matches!(arg_ref, Value::Null) {
                            // v7.39 (round 626) — see the Max arm above.
                            if !ctx.mysql_dialect && min_max_unsupported_type(arg_ref) {
                                return Err(EvalError::TypeMismatch {
                                    detail: format!(
                                        "function min({}) does not exist",
                                        crate::conversions::pg_type_name_for_error_opt(
                                            arg_ref.data_type()
                                        )
                                    ),
                                });
                            }
                            let st = &mut entry.1[i];
                            let upd = match &st.extreme {
                                None => true,
                                Some(prev) => {
                                    extreme_cmp_in(
                                        spec.enum_labels.as_deref(),
                                        spec.arg_collation.as_deref(),
                                        arg_ref,
                                        prev,
                                        ctx.mysql_dialect,
                                    ) == core::cmp::Ordering::Less
                                }
                            };
                            if upd {
                                st.extreme = Some(arg_ref.clone().into_owned());
                            }
                        }
                    }
                    AggKind::AnyValue => {
                        if !matches!(arg_ref, Value::Null) {
                            let st = &mut entry.1[i];
                            if st.extreme.is_none() {
                                st.extreme = Some(arg_ref.clone().into_owned());
                            }
                        }
                    }
                    AggKind::CountStar => {
                        entry.1[i].num.count += 1;
                    }
                    AggKind::Count => {
                        if !matches!(arg_ref, Value::Null) {
                            entry.1[i].num.count += 1;
                        }
                    }
                    AggKind::BoolOr => match arg_ref {
                        Value::Bool(b) => {
                            let st = &mut entry.1[i];
                            st.bool_acc = Some(st.bool_acc.unwrap_or(false) || *b);
                        }
                        Value::Null => {}
                        _ => update_state(
                            &mut entry.1[i],
                            spec.kind,
                            &spec.name,
                            arg_ref,
                            arg2_val.as_ref(),
                            order_keys,
                            spec.enum_labels.as_deref(),
                            spec.arg_collation.as_deref(),
                            ctx.mysql_dialect,
                        )?,
                    },
                    AggKind::BoolAnd => match arg_ref {
                        Value::Bool(b) => {
                            let st = &mut entry.1[i];
                            st.bool_acc = Some(st.bool_acc.unwrap_or(true) && *b);
                        }
                        Value::Null => {}
                        _ => update_state(
                            &mut entry.1[i],
                            spec.kind,
                            &spec.name,
                            arg_ref,
                            arg2_val.as_ref(),
                            order_keys,
                            spec.enum_labels.as_deref(),
                            spec.arg_collation.as_deref(),
                            ctx.mysql_dialect,
                        )?,
                    },
                    _ => {
                        update_state(
                            &mut entry.1[i],
                            spec.kind,
                            &spec.name,
                            arg_ref,
                            arg2_val.as_ref(),
                            order_keys,
                            spec.enum_labels.as_deref(),
                            spec.arg_collation.as_deref(),
                            ctx.mysql_dialect,
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
        let row: &Row<'static> = &row_materialised;
        let group_vals: Vec<Value<'static>> = group_exprs
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
                    // v7.39 (round 370, M4 P4a) — a MySQL folding column
                    // (stored CaseInsensitive) folds case AND accent; a PG
                    // CITEXT column stays ASCII-only.
                    key_vals[i] = Value::text(if ctx.mysql_dialect {
                        spg_storage::mysql_compare_fold(s)
                    } else {
                        s.to_ascii_lowercase()
                    });
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
            // separator per row. v7.39 (round 762, F31-C2) — PG uses
            // the PER-ROW value (element i prefixed by row i's
            // separator, PG18-measured `a<b>b<c>c`); update_state
            // records it alongside the item now (the old note claimed
            // PG "treats it as constant" — measured false).
            let arg2_val = match &spec.arg2 {
                None => None,
                Some(e) => Some(eval_arg(e, row, &ctx)?),
            };
            // v7.24 (round-16 A) — aggregate-internal ORDER BY:
            // evaluate the key tuple against the source row.
            let order_keys: Option<Vec<Value<'static>>> = if spec.order_by.is_empty() {
                None
            } else {
                let mut keys: Vec<Value<'static>> = Vec::with_capacity(spec.order_by.len());
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
                            cmp_order_keys(
                                &spec.order_by,
                                &spec.order_enum_labels,
                                &spec.order_collations,
                                &keys,
                                bk,
                                ctx.mysql_dialect,
                            ) == core::cmp::Ordering::Less
                        }
                    };
                    if better {
                        st.first_best = Some((keys, arg_val.clone().into_owned()));
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
                // v7.37.x (docker-fair DISTA) — single-family fast
                // paths skip encode_key for Text/BigInt/Int.
                let inserted = match &arg_val {
                    Value::Text(s) => entry.1[i].seen.insert(s.to_string()),
                    Value::BigInt(n) => entry.1[i]
                        .seen_int
                        .get_or_insert_with(BTreeSet::new)
                        .insert(*n),
                    Value::Int(n) => entry.1[i]
                        .seen_int
                        .get_or_insert_with(BTreeSet::new)
                        .insert(i64::from(*n)),
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
                spec.enum_labels.as_deref(),
                spec.arg_collation.as_deref(),
                ctx.mysql_dialect,
            )?;
        }
    }
    Ok(order)
}

/// (2a) Build the synthetic per-group schema: `__grp_0..K` then
/// `__agg_0..N`. Group types are probed from the first row; aggregate
/// types from each spec.
fn build_synth_schema(
    rows: AggRows<'_>,
    group_exprs: &[Expr],
    agg_specs: &[AggSpec],
    schema_cols: &[ColumnSchema],
    table_alias: Option<&str>,
    catalog: Option<&spg_storage::Catalog>,
    engine: Option<&crate::Engine>,
) -> Result<Vec<ColumnSchema>, EvalError> {
    let ctx = with_catalog(EvalContext::new(schema_cols, table_alias), catalog, engine);
    // Build synthetic schema: __grp_0..K then __agg_0..N.
    let group_types: Vec<DataType> = if rows.is_empty() {
        // Use Text as a safe stand-in — empty result means schema isn't
        // observable. Avoids needing to evaluate group exprs on no row.
        group_exprs.iter().map(|_| DataType::Text).collect()
    } else {
        let probe = rows.get(0).expect("non-empty checked above");
        let probe_row = probe.as_row();
        let probe: &Row<'static> = &probe_row;
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
        let mut col = ColumnSchema::new(format!("__grp_{i}"), *ty, true);
        // v7.39 (enum order knife) — a bare enum-column group key keeps
        // its enum identity so HAVING comparisons and the grouped-output
        // ORDER BY sort by member order downstream.
        if let Some(Expr::Column(c)) = group_exprs.get(i) {
            let src = schema_cols.iter().find(|sc| sc.name == c.name);
            col.user_enum_type = src.and_then(|sc| sc.user_enum_type.clone());
            // v7.39 (round 686) — and its collation, for the same reason and
            // by the same route. A `__grp_j` column is where a GROUP BY key
            // lives from here on, so anything the downstream ORDER BY needs
            // about the original column has to travel with it. Without this
            // the resolver looks the key up in the synthetic schema, finds
            // `__grp_0` with no collation, and the group-by ordering silently
            // stays byte-wise.
            col.collation_name = src.and_then(|sc| sc.collation_name.clone());
            // v7.38.14 — and the collation ENUM, which is a different field
            // and the one every MySQL text comparison actually reads. The
            // note above carried the NAME and stopped, exactly as round 688
            // did in `join.rs::build_combined_schema`; both left the enum
            // behind, and `ColumnSchema::new` defaults it to `Binary`, which
            // downstream reads as "byte-wise ON PURPOSE" rather than as
            // "unknown". So a `__grp_j` column claimed to be an explicit
            // binary column and `SELECT DISTINCT ... GROUP BY` stopped
            // folding. Sixth field through this hole, second site with the
            // identical shape.
            if let Some(sc) = src {
                col.collation = sc.collation;
            }
        }
        synth_schema.push(col);
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
    order_enum_labels: &[Option<Vec<String>>],
    order_collations: &[Option<alloc::string::String>],
    a: &[Value<'static>],
    b: &[Value<'static>],
    mysql: bool,
) -> core::cmp::Ordering {
    for (k, o) in order_by.iter().enumerate() {
        // v7.39 (enum order knife) — an enum-typed sort key compares by
        // member order; NULLs and non-members keep the generic path.
        if let Some(Some(labels)) = order_enum_labels.get(k)
            && !matches!(&a[k], Value::Null)
            && !matches!(&b[k], Value::Null)
            && let Some(ord) = crate::eval::enum_ord_cmp(labels, &a[k], &b[k])
        {
            let ord = if o.desc { ord.reverse() } else { ord };
            if ord != core::cmp::Ordering::Equal {
                return ord;
            }
            continue;
        }
        // v7.37 (M4 P2) — `ORDER BY BINARY x` forces byte-wise sorting
        // even under the folding MySQL dialect, so a per-key BINARY
        // coercion turns folding back off for that key alone.
        let fold = mysql && !crate::eval::is_binary_coerced(&o.expr);
        // v7.38.18 — the key's declared collation, so the sort inside an
        // aggregate orders a collated column the way the statement's own
        // ORDER BY orders it.
        let coll = order_collations.get(k).and_then(Option::as_deref);
        let cmp = crate::orderby::order_by_value_cmp_coll(
            o.desc,
            o.nulls_first,
            &a[k],
            &b[k],
            fold,
            coll,
        );
        if cmp != core::cmp::Ordering::Equal {
            return cmp;
        }
    }
    core::cmp::Ordering::Equal
}

#[allow(clippy::too_many_arguments)]
fn finalize_synth_rows(
    order: &[(Vec<Value<'static>>, Vec<AggState>)],
    agg_specs: &[AggSpec],
    synth_schema: &[ColumnSchema],
    rows: AggRows<'_>,
    schema_cols: &[ColumnSchema],
    table_alias: Option<&str>,
    catalog: Option<&spg_storage::Catalog>,
    engine: Option<&crate::Engine>,
    runner: Option<&dyn crate::ParallelRunner>,
) -> Result<Vec<Row<'static>>, EvalError> {
    let ctx = with_catalog(EvalContext::new(schema_cols, table_alias), catalog, engine);
    // v7.39 (round 747) — GROUP-parallel finalize for the collection
    // aggregates. `string_agg(s, ',' ORDER BY id) GROUP BY g` sorted
    // and joined every group's items serially — the panel's last
    // >=2.0x cell. Groups are independent; shards produce their row
    // ranges in group order and concatenate. Admission: every spec a
    // collection kind (their finalize reads items/keys/separator and
    // the dialect only — nothing that needs the engine hook), no
    // ordered-set / first_ordered / regression shapes.
    let collections_only = agg_specs.iter().all(|s| {
        matches!(
            classify_agg_name(&s.name),
            AggKind::StringAgg | AggKind::ArrayAgg | AggKind::JsonAgg
        ) && !s.first_ordered
            && !is_within_group_name(&s.name)
    });
    if collections_only
        && order.len() >= 16
        && let Some(r) = runner
    {
        let group_len_probe = order.first().map(|(g, _)| g.len()).unwrap_or(0);
        let _ = group_len_probe;
        let n_shards = (order.len() / 8).clamp(2, 8);
        let chunk = order.len().div_ceil(n_shards);
        type ShardOut = Result<Vec<Row<'static>>, EvalError>;
        let mysql = ctx.mysql_dialect;
        let style = ctx.render_style;
        let results = r.run_shards(n_shards, &|si| {
            let lo = si * chunk;
            let hi = ((si + 1) * chunk).min(order.len());
            let mut sctx = EvalContext::new(schema_cols, table_alias);
            sctx.mysql_dialect = mysql;
            sctx.render_style = style;
            let run = || -> ShardOut {
                let mut out: Vec<Row<'static>> = Vec::with_capacity(hi - lo);
                for (gvals, states) in &order[lo..hi] {
                    out.push(finalize_one_group(
                        gvals,
                        states,
                        agg_specs,
                        synth_schema,
                        &sctx,
                    )?);
                }
                Ok(out)
            };
            alloc::boxed::Box::new(run())
        });
        let mut synth_rows: Vec<Row<'static>> = Vec::with_capacity(order.len());
        for boxed in results {
            let shard = boxed
                .downcast::<ShardOut>()
                .expect("runner echoes the closure's box");
            synth_rows.extend((*shard)?);
        }
        return Ok(synth_rows);
    }
    // v7.32 (round-29) — ordered-set direct arguments (the percentile
    // fraction) are constant per PG, so evaluate each once up front.
    let direct_arg_vals: Vec<Option<Value>> = agg_specs
        .iter()
        .map(|spec| match (&spec.direct_arg, rows.first().as_ref()) {
            (Some(e), Some(r)) => eval::eval_expr(e, &r.as_row(), &ctx).map(Some),
            _ => Ok(None),
        })
        .collect::<Result<_, _>>()?;
    // v7.39 (read01 orderedsetaggs.c) — the remaining hypothetical direct
    // arguments of a multi-key call, evaluated once like the first.
    let direct_extra_vals: Vec<Vec<Value>> = agg_specs
        .iter()
        .map(|spec| match rows.first().as_ref() {
            Some(r) if !spec.direct_args_extra.is_empty() => spec
                .direct_args_extra
                .iter()
                .map(|e| eval::eval_expr(e, &r.as_row(), &ctx))
                .collect(),
            _ => Ok(Vec::new()),
        })
        .collect::<Result<_, _>>()?;

    // Materialise synthetic rows (insertion order = `order`).
    let mut synth_rows: Vec<Row<'static>> = Vec::new();
    for (gvals, states) in order {
        let mut values: Vec<Value<'static>> = Vec::with_capacity(synth_schema.len());
        // The synth schema is [group keys…, aggregates…]; the aggregate at
        // index `i` therefore sits at `group_len + i`.
        let group_len = gvals.len();
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
            let kw = agg_specs[i].order_by.len();
            let st_final: &AggState = if kw > 0 && st.item_keys.len() == st.items.len() * kw {
                let mut idx: Vec<usize> = (0..st.items.len()).collect();
                let ob = &agg_specs[i].order_by;
                idx.sort_by(|&x, &y| {
                    cmp_order_keys(
                        ob,
                        &agg_specs[i].order_enum_labels,
                        &agg_specs[i].order_collations,
                        &st.item_keys[x * kw..(x + 1) * kw],
                        &st.item_keys[y * kw..(y + 1) * kw],
                        ctx.mysql_dialect,
                    )
                });
                // Permute by MOVE out of the clone — the old form
                // cloned every item a second time on top of
                // `st.clone()`'s first (5000 Strings twice per group).
                let mut sorted = st.clone();
                let mut new_items: Vec<Value<'static>> = Vec::with_capacity(idx.len());
                for &j in &idx {
                    new_items.push(core::mem::replace(&mut sorted.items[j], Value::Null));
                }
                // v7.39 (round 762, F31-C2) — the per-row separators
                // travel with their items through the sort.
                if sorted.item_seps.len() == sorted.items.len() {
                    let mut new_seps: Vec<Option<String>> = Vec::with_capacity(idx.len());
                    for &j in &idx {
                        new_seps.push(core::mem::take(&mut sorted.item_seps[j]));
                    }
                    sorted.item_seps = new_seps;
                }
                sorted.items = new_items;
                st_sorted = sorted;
                &st_sorted
            } else if agg_specs[i].distinct && st.items.len() > 1 {
                // v7.39 (round 257) — PG dedups a DISTINCT aggregate by
                // SORTING its input, so the collection aggregates emit
                // their values in sort order (probed across array_agg /
                // string_agg / json_agg, ints and text, NULLs last):
                // `array_agg(DISTINCT x)` over 2,1,2 is `{1,2}`, where
                // SPG kept first-seen order and answered `{2,1}`. An
                // explicit ORDER BY takes the branch above instead, and
                // the scalar aggregates (count / sum / …) are
                // order-insensitive, so this only moves the collections.
                // v7.39 (round 258) — an ENUM input sorts by MEMBER
                // ORDER, not by its text (`{sad,ok,happy}`, not
                // `{happy,ok,sad}`); `spec.enum_labels` already
                // carries the aggregate argument's labels for exactly
                // this. Round 257 shipped this sort with the generic
                // value comparison and regressed enum columns.
                let labels = agg_specs[i].enum_labels.as_deref();
                let mut sorted = st.clone();
                // v7.39 (round 762, F31-C2) — DISTINCT re-sorts items
                // alone; per-row separators cannot follow, so the
                // constant-separator path applies (the last row's).
                sorted.item_seps.clear();
                sorted.items.sort_by(|a, b| {
                    if let Some(labels) = labels
                        && !matches!(a, Value::Null)
                        && !matches!(b, Value::Null)
                        && let Some(ord) = crate::eval::enum_ord_cmp(labels, a, b)
                    {
                        return ord;
                    }
                    crate::order_by_value_cmp_in(false, Some(false), a, b, ctx.mysql_dialect)
                });
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
                    &direct_extra_vals[i],
                    &agg_specs[i].order_by,
                    &agg_specs[i].order_collations,
                    ctx.mysql_dialect,
                )?
            } else {
                finalize(&agg_specs[i].name, st_final, ctx.mysql_dialect)
            };
            // v7.39 (round 327, V44) — keep the zone identity. SPG carries a
            // timestamptz at runtime as `Value::Timestamp`, so the array
            // `array_agg` builds is a `TimestampArray` and `pg_typeof`
            // answered `timestamp without time zone[]` for
            // `array_agg(timestamptz_col)`. The STATIC type in the synth
            // schema already knows better (`infer_agg_type` maps
            // Timestamptz ⇒ TimestamptzArray); re-tag the value to match
            // it. Third code path in this family — V31 fixed the array
            // constructor, V43 the literal cast.
            let v = match (v, synth_schema.get(group_len + i).map(|c| c.ty)) {
                (Value::TimestampArray(items), Some(DataType::TimestamptzArray)) => {
                    Value::TimestamptzArray(items)
                }
                (v, _) => v,
            };
            values.push(v);
        }
        synth_rows.push(Row::new(values));
    }
    Ok(synth_rows)
}

/// v7.39 (round 747) — one group's synth row for the COLLECTION
/// aggregates (string_agg / array_agg / json_agg): the ordered/distinct
/// sort branches verbatim from the serial loop, then `finalize`. The
/// group-parallel path calls this; admission guarantees no
/// first_ordered / within-group / timestamptz-retag shapes reach it
/// (json/array of timestamptz retag is still applied for safety).
fn finalize_one_group(
    gvals: &[Value<'static>],
    states: &[AggState],
    agg_specs: &[AggSpec],
    synth_schema: &[ColumnSchema],
    ctx: &EvalContext<'_>,
) -> Result<Row<'static>, EvalError> {
    let group_len = gvals.len();
    let mut values: Vec<Value<'static>> = Vec::with_capacity(synth_schema.len());
    values.extend(gvals.iter().cloned());
    for (i, st) in states.iter().enumerate() {
        let st_sorted;
        let kw = agg_specs[i].order_by.len();
        let st_final: &AggState = if kw > 0 && st.item_keys.len() == st.items.len() * kw {
            let mut idx: Vec<usize> = (0..st.items.len()).collect();
            let ob = &agg_specs[i].order_by;
            idx.sort_by(|&x, &y| {
                cmp_order_keys(
                    ob,
                    &agg_specs[i].order_enum_labels,
                    &agg_specs[i].order_collations,
                    &st.item_keys[x * kw..(x + 1) * kw],
                    &st.item_keys[y * kw..(y + 1) * kw],
                    ctx.mysql_dialect,
                )
            });
            let mut sorted = st.clone();
            let mut new_items: Vec<Value<'static>> = Vec::with_capacity(idx.len());
            for &j in &idx {
                new_items.push(core::mem::replace(&mut sorted.items[j], Value::Null));
            }
            // v7.39 (round 762, F31-C2) — separators travel with items.
            if sorted.item_seps.len() == sorted.items.len() {
                let mut new_seps: Vec<Option<String>> = Vec::with_capacity(idx.len());
                for &j in &idx {
                    new_seps.push(core::mem::take(&mut sorted.item_seps[j]));
                }
                sorted.item_seps = new_seps;
            }
            sorted.items = new_items;
            st_sorted = sorted;
            &st_sorted
        } else if agg_specs[i].distinct && st.items.len() > 1 {
            let labels = agg_specs[i].enum_labels.as_deref();
            let mut sorted = st.clone();
            // v7.39 (round 762, F31-C2) — see the sibling branch above.
            sorted.item_seps.clear();
            sorted.items.sort_by(|a, b| {
                if let Some(labels) = labels
                    && !matches!(a, Value::Null)
                    && !matches!(b, Value::Null)
                    && let Some(ord) = crate::eval::enum_ord_cmp(labels, a, b)
                {
                    return ord;
                }
                crate::order_by_value_cmp_in(false, Some(false), a, b, ctx.mysql_dialect)
            });
            st_sorted = sorted;
            &st_sorted
        } else {
            st
        };
        let v = finalize(&agg_specs[i].name, st_final, ctx.mysql_dialect);
        let v = match (v, synth_schema.get(group_len + i).map(|c| c.ty)) {
            (Value::TimestampArray(items), Some(DataType::TimestamptzArray)) => {
                Value::TimestamptzArray(items)
            }
            (v, _) => v,
        };
        values.push(v);
    }
    Ok(Row::new(values))
}

/// (3) Rewrite the user's SELECT items + HAVING to reference the
/// synthetic columns, filter groups by HAVING, and project each
/// surviving group into an output row. The synth rows ride alongside
/// (`kept_synth`) so post-LIMIT deferred subqueries can evaluate later.
#[allow(clippy::too_many_lines)]
fn project_groups(
    synth_rows: Vec<Row<'static>>,
    stmt: &SelectStatement,
    group_exprs: &[Expr],
    agg_specs: &[AggSpec],
    synth_schema: &[ColumnSchema],
    correlated_eval: Option<CorrelatedEval<'_>>,
    defer_projection: bool,
    catalog: Option<&spg_storage::Catalog>,
    mysql: bool,
) -> Result<Projection, EvalError> {
    // Rewrite the user's SELECT items + ORDER BY to reference synthetic
    // columns. After rewriting, every remaining `Expr::Column` must
    // resolve against the synthetic schema (i.e. must have been a GROUP
    // BY expression).
    let columns: Vec<ColumnSchema> = stmt
        .items
        .iter()
        .map(|item| match item {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                Err(EvalError::TypeMismatch {
                    detail: "SELECT * with aggregates is not supported".into(),
                })
            }
            SelectItem::Expr { expr, alias } => {
                let rewritten = rewrite_expr(expr, group_exprs, agg_specs);
                let name = alias
                    .clone()
                    .unwrap_or_else(|| crate::select::default_output_name(expr, mysql));
                // v7.38.14 — the type is looked up in the synthetic schema
                // here; the COLLATION has to travel by the same route or the
                // output column claims `ColumnSchema::new`'s default, which
                // is `Binary` and reads downstream as "byte-wise on
                // purpose". That is what made `SELECT DISTINCT ... GROUP BY`
                // stop folding: the de-duplication asked the output schema
                // and the output schema had forgotten.
                //
                // Third site with this exact shape in one release, after
                // `join.rs::build_combined_schema` and `synth_schema` above.
                // Each one hand-picks which attributes survive; none picks
                // all of them. See S4 of the v7.38.14 roadmap.
                let mut col =
                    ColumnSchema::new(name, agg_or_group_type(&rewritten, synth_schema), true);
                if let Expr::Column(c) = &rewritten
                    && let Some(sc) = synth_schema
                        .iter()
                        .find(|sc| sc.name.eq_ignore_ascii_case(&c.name))
                {
                    col.collation = sc.collation;
                    col.collation_name.clone_from(&sc.collation_name);
                }
                Ok(col)
            }
        })
        .collect::<Result<_, _>>()?;

    // Project per synthetic row. HAVING filters out groups *before*
    // we keep the projected row — same semantics as PG: HAVING runs
    // against the aggregated row (so `HAVING count(*) > 1` works) and
    // sees only group-by'd columns plus aggregate values.
    let mut synth_ctx = EvalContext::new(synth_schema, None);
    // v7.39 (enum order knife) — HAVING comparisons over enum group keys
    // need the catalog for member-order semantics (both the compile-time
    // Subtree fallback witness and the eval hook read it).
    if let Some(cat) = catalog {
        synth_ctx = synth_ctx.with_catalog(cat);
    }
    // v7.39 (round 404) — a MySQL session lets HAVING name a SELECT alias.
    // Build the (alias, expr) map from renaming SELECT items, then subst
    // before the aggregate rewrite.
    let having_aliases: Vec<(String, Expr)> = if mysql {
        stmt.items
            .iter()
            .filter_map(|it| match it {
                SelectItem::Expr {
                    expr,
                    alias: Some(a),
                } if !matches!(expr, Expr::Column(c)
                    if c.qualifier.is_none() && c.name.eq_ignore_ascii_case(a)) =>
                {
                    Some((a.clone(), expr.clone()))
                }
                _ => None,
            })
            .collect()
    } else {
        Vec::new()
    };
    let having_rewritten = stmt.having.as_ref().map(|h| {
        let h = if having_aliases.is_empty() {
            h.clone()
        } else {
            substitute_having_aliases(h.clone(), &having_aliases)
        };
        rewrite_expr(&h, group_exprs, agg_specs)
    });
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
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => None,
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
    // v7.39 (round 621) — which items are set-returning, after the rewrite
    // (so `unnest(array_agg(x))` is seen as the SRF it is, over a synthetic
    // aggregate column). Only the builtin SRFs are recognised here; a user
    // `RETURNS SETOF` function inside an aggregate query keeps the old error,
    // because running its body needs the executor and this is not it.
    let srf_items: Vec<bool> = items_rewritten
        .iter()
        .map(|r| {
            r.as_ref()
                .is_some_and(|e| crate::select::top_level_srf_kind(e).is_some())
        })
        .collect();
    let any_srf = srf_items.iter().any(|b| *b);
    let mut kept_synth: Vec<Row<'static>> = Vec::new();
    let mut out_rows: Vec<Row<'static>> = Vec::new();
    let mut stack: Vec<Value<'static>> = Vec::new();
    for srow in synth_rows {
        if let Some(hc) = &having_compiled {
            let cond = eval::eval_compiled(hc, &srow, &synth_ctx, &mut stack)?;
            if !crate::eval::predicate_is_true(&cond, "HAVING", synth_ctx.mysql_dialect)? {
                continue;
            }
        } else if let Some(h) = &having_rewritten {
            let cond = match correlated_eval {
                Some(f) if crate::expr_has_subquery(h) => f(h, &srow, &synth_ctx)?,
                _ => eval::eval_expr(h, &srow, &synth_ctx)?,
            };
            if !crate::eval::predicate_is_true(&cond, "HAVING", synth_ctx.mysql_dialect)? {
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
        let mut values: Vec<Value<'static>> = Vec::with_capacity(columns.len());
        for (i, rewritten) in items_rewritten.iter().enumerate() {
            let Some(rewritten) = rewritten else { continue };
            if deferred.iter().any(|(c, _)| *c == i) {
                values.push(Value::Null);
                continue;
            }
            // v7.39 (round 621) — a SET-RETURNING item is collected as its
            // whole list; the rows it makes are built after the loop.
            if srf_items[i] {
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
        if any_srf {
            // v7.39 (round 621) — the aggregate's own output row is what a
            // target-list SRF expands over. `SELECT unnest(ARRAY[1,2]),
            // count(*) FROM t` answered `function unnest(integer[]) does not
            // exist`, because this projection evaluates each item scalarly and
            // there is exactly one row per group to put it in. PG answers two
            // rows, both carrying the same count — and the shape that matters
            // most is `unnest(array_agg(x))`, where the SRF's ARGUMENT is the
            // aggregate.
            //
            // Several SRFs in one list expand in LOCKSTEP with the shorter
            // padded to NULL, which is round 67's rule for every other path.
            let mut lists: Vec<Vec<Value<'static>>> = Vec::with_capacity(items_rewritten.len());
            for (i, rewritten) in items_rewritten.iter().enumerate() {
                match (srf_items[i], rewritten) {
                    (true, Some(r)) => {
                        lists.push(
                            crate::select::top_level_srf_output(r, &srow, &synth_ctx).map_err(
                                |e| match e {
                                    crate::EngineError::Eval(ev) => ev,
                                    other => EvalError::TypeMismatch {
                                        detail: alloc::format!("{other}"),
                                    },
                                },
                            )?,
                        );
                    }
                    _ => lists.push(Vec::new()),
                }
            }
            let n = lists.iter().map(Vec::len).max().unwrap_or(0);
            for k in 0..n {
                let mut vals = values.clone();
                for (i, list) in lists.iter().enumerate() {
                    if srf_items[i]
                        && let Some(slot) = vals.get_mut(i)
                    {
                        *slot = list.get(k).cloned().unwrap_or(Value::Null);
                    }
                }
                kept_synth.push(srow.clone());
                out_rows.push(Row::new(vals));
            }
            continue;
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
    out_columns: &[ColumnSchema],
    order_by: &[spg_sql::ast::OrderBy],
    order_rewritten: &[Expr],
    mut kept_synth: Vec<Row<'static>>,
    mut out_rows: Vec<Row<'static>>,
    correlated_eval: Option<CorrelatedEval<'_>>,
    keep_n: Option<usize>,
    catalog: Option<&spg_storage::Catalog>,
    mysql: bool,
) -> Result<(Vec<Row<'static>>, Vec<Row<'static>>), EvalError> {
    let mut synth_ctx = EvalContext::new(synth_schema, None);
    if let Some(cat) = catalog {
        synth_ctx = synth_ctx.with_catalog(cat);
    }
    // v7.39 (enum order knife) — per-key member labels when the rewritten
    // sort key is an enum-typed column (`__grp_K` carrying user_enum_type).
    let key_enum_labels: Vec<Option<&[String]>> = order_rewritten
        .iter()
        .map(|e| crate::eval::expr_enum_labels(e, synth_schema, catalog))
        .collect();
    // v7.39 (round 686) — per-key declared collation, built exactly like the
    // enum labels above because it is the same kind of thing: metadata the
    // comparator needs, resolved once per sort from the key expression.
    //
    // Located by forcing this call site to reverse and watching
    // `GROUP BY loc ORDER BY loc` flip. Rounds 682 and 685 wired eleven
    // sites between them without doing that, and none was on the path.
    let key_colls: Vec<Option<alloc::string::String>> = order_rewritten
        .iter()
        .map(|e| {
            let spg_sql::ast::Expr::Column(c) = e else {
                return None;
            };
            let pos = crate::eval::find_column_pos(c, &synth_ctx)?;
            let name = synth_schema.get(pos)?.collation_name.clone()?;
            crate::collate::is_supported(&name).then_some(name)
        })
        .collect();
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
    // v7.37 (round 1000) — a sort key that names an OUTPUT column.
    //
    // `ORDER BY 1` over a set-returning item does not substitute the
    // item's expression: round 80 resolved it to the item's output NAME
    // instead, because a positional key means the Nth OUTPUT column and
    // substituting the expression would make the key "the whole set",
    // evaluated once per group, which silently sorted nothing. The
    // non-aggregate paths then evaluate that name against the output
    // schema.
    //
    // This one evaluated it against the SYNTHETIC schema, which carries
    // `__agg_N` / `__grp_K` and no output aliases, so
    // `SELECT unnest(ARRAY[1,2]) AS u, count(*) … GROUP BY g ORDER BY 1`
    // answered `column "u" does not exist` — a query PG18.4 answers.
    // Spelling it `ORDER BY u` failed differently and for the same
    // reason: the alias resolved to the expression, and a set-returning
    // call cannot be evaluated scalarly on a group row.
    //
    // So: a key that names an output column and NOTHING in the synthetic
    // schema is read from the projected row, where expansion has already
    // put the per-row value. Synthetic names keep precedence, so nothing
    // that resolved before resolves differently now.
    let out_key_idx: Vec<Option<usize>> = order_rewritten
        .iter()
        .map(|e| {
            let spg_sql::ast::Expr::Column(c) = e else {
                return None;
            };
            if c.qualifier.is_some() || crate::eval::find_column_pos(c, &synth_ctx).is_some() {
                return None;
            }
            out_columns
                .iter()
                .position(|oc| oc.name.eq_ignore_ascii_case(&c.name))
        })
        .collect();
    let mut keystack: Vec<Value<'static>> = Vec::new();
    let mut tagged: Vec<(Vec<Value<'static>>, Row, Row)> = Vec::with_capacity(kept_synth.len());
    for (s, o) in kept_synth.into_iter().zip(out_rows) {
        let mut keys = Vec::with_capacity(order_rewritten.len());
        for (i, (e, oc)) in order_rewritten.iter().zip(&order_compiled).enumerate() {
            if let Some(oi) = out_key_idx[i] {
                keys.push(o.values.get(oi).cloned().unwrap_or(Value::Null));
                continue;
            }
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
    let cmp = |a: &(Vec<Value<'static>>, Row, Row), b: &(Vec<Value<'static>>, Row, Row)| {
        use core::cmp::Ordering;
        for (i, (ka, kb)) in a.0.iter().zip(b.0.iter()).enumerate() {
            let (desc, nf) = keys_meta[i];
            // v7.39 (enum order knife) — enum keys sort by member order.
            if let Some(Some(labels)) = key_enum_labels.get(i)
                && !matches!(ka, Value::Null)
                && !matches!(kb, Value::Null)
                && let Some(ord) = crate::eval::enum_ord_cmp(labels, ka, kb)
            {
                let ord = if desc { ord.reverse() } else { ord };
                if ord != Ordering::Equal {
                    return ord;
                }
                continue;
            }
            let c = crate::orderby::order_by_value_cmp_coll(
                desc,
                nf,
                ka,
                kb,
                mysql,
                key_colls.get(i).and_then(|c| c.as_deref()),
            );
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
                | "any_value" | "range_agg" | "range_intersect_agg"
                // v7.17.0 — boolean aggregates also take exactly
                // one arg. `every` is an alias normalised inside
                // collect_aggregates / rewrite_expr.
                | "bool_and" | "bool_or" | "every"
                // v7.32 (round-29) — statistical + bitwise aggregates
                // + single-arg JSON aggregate.
                | "stddev" | "stddev_samp" | "stddev_pop"
                | "variance" | "var_samp" | "var_pop"
                | "bit_and" | "bit_or" | "bit_xor"
                | "json_agg" | "jsonb_agg" | "xmlagg"
                | "json_arrayagg" | "json_agg_strict" | "jsonb_agg_strict" => Some(1),
                // v7.39 (round 354, M12) — GROUP_CONCAT takes any number of
                // arguments: MySQL concatenates them PER ROW
                // (`GROUP_CONCAT(n, ':', t)` is `3:c,1:a,…`, measured), and
                // the parser lowers a `SEPARATOR '<s>'` tail onto the last
                // one. Fixing the arity at 1 refused both.
                "group_concat" => None,
                // v7.32 (round-29) — two-argument aggregates: string_agg,
                // the regression family f(Y, X), and json_object_agg.
                "string_agg"
                | "covar_pop" | "covar_samp" | "corr"
                | "regr_count" | "regr_avgx" | "regr_avgy" | "regr_slope"
                | "regr_intercept" | "regr_r2" | "regr_sxx" | "regr_syy" | "regr_sxy"
                | "json_object_agg" | "jsonb_object_agg"
                | "json_objectagg"
                | "json_object_agg_strict" | "jsonb_object_agg_strict"
                | "json_object_agg_unique" | "jsonb_object_agg_unique"
                | "json_object_agg_unique_strict" | "jsonb_object_agg_unique_strict" => Some(2),
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
        | Expr::IsNull { expr, .. }
        | Expr::BoolTest { expr, .. } = e
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

/// v7.39 (round 615) — the exact pair the finaliser reads: the BigNumeric
/// accumulator combined with whatever the i128 one still holds. Read-only,
/// because finalisation only borrows the state.
fn stddev_exact_pair(
    st: &AggState,
) -> Option<(
    spg_storage::bignum::BigNumeric,
    spg_storage::bignum::BigNumeric,
)> {
    use spg_storage::bignum::BigNumeric as BN;
    let fast =
        (!st.stddev_i_spent && (st.stddev_i_sum != 0 || st.stddev_i_sum_sq != 0)).then(|| {
            (
                BN::from_i128(st.stddev_i_sum, 0),
                BN::from_i128(st.stddev_i_sum_sq, 0),
            )
        });
    match (st.stddev_sum.as_ref(), st.stddev_sum_sq.as_ref(), fast) {
        (Some(s), Some(sq), Some((fs, fsq))) => Some((s.add(&fs), sq.add(&fsq))),
        (Some(s), Some(sq), None) => Some((s.clone(), sq.clone())),
        (None, None, Some(pair)) => Some(pair),
        _ => None,
    }
}

/// v7.39 (round 615) — fold the i128 Σx / Σx² into the exact BigNumeric
/// pair and retire the fast accumulator. Called once when an input needs the
/// slow path, and once at finalisation; both are idempotent because the fast
/// pair is zeroed as it is spent.
fn spend_stddev_i128(st: &mut AggState) {
    if st.stddev_i_spent {
        return;
    }
    st.stddev_i_spent = true;
    if st.stddev_i_sum == 0 && st.stddev_i_sum_sq == 0 {
        // Nothing accumulated: leave the pair as it was (None means "no
        // exact input yet", which the finaliser reads).
        return;
    }
    use spg_storage::bignum::BigNumeric as BN;
    let sum = BN::from_i128(st.stddev_i_sum, 0);
    let sum_sq = BN::from_i128(st.stddev_i_sum_sq, 0);
    st.stddev_sum = Some(st.stddev_sum.as_ref().map_or(sum.clone(), |s| s.add(&sum)));
    st.stddev_sum_sq = Some(
        st.stddev_sum_sq
            .as_ref()
            .map_or(sum_sq.clone(), |s| s.add(&sum_sq)),
    );
}

fn collect_aggregates(e: &Expr, out: &mut Vec<AggSpec>) {
    match e {
        Expr::NamedArg { expr, .. } => collect_aggregates(expr, out),
        Expr::Variadic(expr) => collect_aggregates(expr, out),
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
                    let (arg, direct_arg, direct_args_extra) = if ordered_set {
                        (
                            order_by.first().map(|o| o.expr.clone()),
                            args.first().cloned(),
                            args.iter().skip(1).cloned().collect(),
                        )
                    } else {
                        (args.first().cloned(), None, Vec::new())
                    };
                    let spec = AggSpec {
                        kind: classify_agg_name(&canonical),
                        enum_labels: None,
                        arg_collation: None,
                        order_enum_labels: Vec::new(),
                        order_collations: Vec::new(),
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
                        direct_args_extra,
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
                            && s.direct_args_extra == spec.direct_args_extra
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
                    enum_labels: None,
                    arg_collation: None,
                    order_enum_labels: Vec::new(),
                    order_collations: Vec::new(),
                    name: canonical,
                    arg: arg.clone(),
                    arg2: arg2.clone(),
                    distinct: false,
                    order_by: Vec::new(),
                    filter: None,
                    direct_arg: None,
                    direct_args_extra: Vec::new(),
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
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::BoolTest { expr, .. }
        | Expr::FieldAccess { base: expr, .. } => {
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
        | Expr::RowInSubquery { .. }
        | Expr::RowCmpSubquery { .. }
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
                    enum_labels: None,
                    arg_collation: None,
                    order_enum_labels: Vec::new(),
                    order_collations: Vec::new(),
                    name: "array_agg".to_string(),
                    arg: Some(arg.clone()),
                    arg2: None,
                    distinct: false,
                    order_by: order_by.to_vec(),
                    filter: filter.cloned(),
                    direct_arg: None,
                    direct_args_extra: Vec::new(),
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
        Expr::ArraySlice { target, lo, hi } => {
            collect_aggregates(target, out);
            if let Some(l) = lo {
                collect_aggregates(l, out);
            }
            if let Some(h) = hi {
                collect_aggregates(h, out);
            }
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

pub(crate) fn update_state(
    st: &mut AggState,
    kind: AggKind,
    name: &str,
    v: &Value<'_>,
    arg2: Option<&Value<'_>>,
    order_keys: Option<Vec<Value<'static>>>,
    enum_labels: Option<&[String]>,
    // v7.39 (round 690) — the argument column's collation, beside
    // `enum_labels` because it is the same kind of fact about the argument.
    arg_collation: Option<&str>,
    mysql: bool,
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
        AggKind::CountStar => st.num.count += 1,
        AggKind::Count => {
            if !is_null {
                st.num.count += 1;
            }
        }
        AggKind::Sum | AggKind::Avg => {
            // v7.39 (round 665) — was a hand-copied duplicate of `acc_cell`,
            // arm for arm, down to the wording of the type error. Verified
            // equivalent before collapsing: same nine variants, same error,
            // and the two apparent differences are both unobservable — this
            // one counted before the match so a value that errors bumped the
            // count first (the error aborts the query, so it is discarded),
            // and its `is_null` early return is literally
            // `matches!(v, Value::Null)`, which is the arm `acc_cell` has.
            //
            // Round 626 had to add a SMALLINT arm HERE that the other three
            // copies already carried; `SELECT sum(x)` over a smallint column
            // answered "sum/avg need numeric, got smallint" until then. That
            // is the failure mode this collapse removes.
            acc_cell(&mut st.num, v)?;
        }
        AggKind::Min => {
            if is_null {
                return Ok(());
            }
            if !mysql && min_max_unsupported_type(v) {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "function min({}) does not exist",
                        crate::conversions::pg_type_name_for_error_opt(v.data_type())
                    ),
                });
            }
            match &st.extreme {
                None => st.extreme = Some(v.clone().into_owned()),
                Some(cur) => {
                    if extreme_cmp_in(enum_labels, arg_collation, v, cur, mysql)
                        == core::cmp::Ordering::Less
                    {
                        st.extreme = Some(v.clone().into_owned());
                    }
                }
            }
        }
        AggKind::AnyValue => {
            if is_null {
                return Ok(());
            }
            if st.extreme.is_none() {
                st.extreme = Some(v.clone().into_owned());
            }
        }
        AggKind::RangeAgg => {
            if is_null {
                return Ok(());
            }
            let Value::Range {
                kind,
                lower,
                upper,
                lower_inc,
                upper_inc,
                empty,
            } = v
            else {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "range_agg requires a range value, got {}",
                        crate::conversions::pg_type_name_for_error_opt(v.data_type())
                    ),
                });
            };
            // Initialise the accumulator on first sight (even for
            // an empty range, so all-empty groups finalize to {}).
            if st.extreme.is_none() {
                st.extreme = Some(Value::Multirange {
                    kind: *kind,
                    ranges: alloc::vec::Vec::new(),
                });
            }
            if !empty && let Some(Value::Multirange { ranges, .. }) = &mut st.extreme {
                ranges.push(spg_storage::RangeSpan {
                    lower: lower.clone(),
                    upper: upper.clone(),
                    lower_inc: *lower_inc,
                    upper_inc: *upper_inc,
                    empty: false,
                });
            }
        }
        AggKind::RangeIntersectAgg => {
            if is_null {
                return Ok(());
            }
            if !matches!(v, Value::Range { .. }) {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "range_intersect_agg requires a range value, got {}",
                        crate::conversions::pg_type_name_for_error_opt(v.data_type())
                    ),
                });
            }
            match &st.extreme {
                None => st.extreme = Some(v.clone().into_owned()),
                Some(prev) => {
                    st.extreme = Some(range_intersect(prev, &v.clone().into_owned()));
                }
            }
        }
        AggKind::Max => {
            if is_null {
                return Ok(());
            }
            if !mysql && min_max_unsupported_type(v) {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "function max({}) does not exist",
                        crate::conversions::pg_type_name_for_error_opt(v.data_type())
                    ),
                });
            }
            match &st.extreme {
                None => st.extreme = Some(v.clone().into_owned()),
                Some(cur) => {
                    if extreme_cmp_in(enum_labels, arg_collation, v, cur, mysql)
                        == core::cmp::Ordering::Greater
                    {
                        st.extreme = Some(v.clone().into_owned());
                    }
                }
            }
        }
        // v7.17.0 — string_agg(value, separator). NULL value is
        // skipped (PG aggregate-skip-null). v7.39 (round 762,
        // F31-C2) — the separator is PER ROW in PG (the old note's
        // "using the last value at finalize" claim was measured
        // false): each surviving item records its own row's
        // separator in `item_seps`; the `separator` snapshot stays
        // for the constant-path consumers. count is bumped so we can
        // distinguish "empty group → NULL" from "all-NULL group →
        // NULL".
        AggKind::StringAgg => {
            let has_arg2 = arg2.is_some();
            if let Some(sep) = arg2
                && let Value::Text(s) = sep
            {
                st.separator = Some(s.to_string());
            }
            if is_null {
                return Ok(());
            }
            // Text collects as-is; other scalars coerce to their
            // text rendering (MySQL group_concat semantics — also
            // matches PG's cast-then-aggregate idiom for
            // string_agg(v::text, sep)).
            let rendered = render_string_agg_item(v);
            if let Some(item) = rendered {
                st.items.push(item);
                // v7.39 (round 762, F31-C2) — the row's own separator
                // rides with its item (NULL separator → None → empty).
                if has_arg2 {
                    st.item_seps.push(match arg2 {
                        Some(Value::Text(sp)) => Some(sp.to_string()),
                        _ => None,
                    });
                }
                if let Some(k) = order_keys {
                    st.item_keys.extend(k);
                }
                st.num.count += 1;
            } else {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "string_agg requires text value, got {}",
                        crate::conversions::pg_type_name_for_error_opt(v.data_type())
                    ),
                });
            }
        }
        // v7.17.0 — array_agg(value). Unlike string_agg, NULL
        // elements are KEPT in the array (PG behaviour); the
        // result is NULL only when ZERO rows fed in. Element type
        // is locked from the first row's value type; subsequent
        // rows must match (PG also rejects mixed-type array_agg).
        AggKind::ArrayAgg => {
            st.items.push(v.clone().into_owned());
            if let Some(k) = order_keys {
                st.item_keys.extend(k);
            }
            st.num.count += 1;
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
                        detail: format!(
                            "bool_and requires bool, got {}",
                            crate::conversions::pg_type_name_for_error_opt(other.data_type())
                        ),
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
                        detail: format!(
                            "bool_or requires bool, got {}",
                            crate::conversions::pg_type_name_for_error_opt(other.data_type())
                        ),
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
            // v7.38 (read01) — keep an exact NUMERIC Σx / Σx² alongside the f64
            // pair for as long as every input is exact; a float input abandons it.
            if !st.stddev_saw_float {
                // v7.39 (round 615) — an integer input stays in i128, which is
                // exact and allocates nothing. Anything else, or an overflow,
                // spends the fast accumulator into the BigNumeric pair and
                // takes the old path from there.
                let as_int = match v {
                    Value::SmallInt(n) => Some(i128::from(*n)),
                    Value::Int(n) => Some(i128::from(*n)),
                    Value::BigInt(n) => Some(i128::from(*n)),
                    _ => None,
                };
                let folded = if st.stddev_i_spent {
                    None
                } else if let Some(x) = as_int {
                    match (
                        st.stddev_i_sum.checked_add(x),
                        x.checked_mul(x)
                            .and_then(|xx| st.stddev_i_sum_sq.checked_add(xx)),
                    ) {
                        (Some(s), Some(sq)) => {
                            st.stddev_i_sum = s;
                            st.stddev_i_sum_sq = sq;
                            Some(())
                        }
                        _ => None,
                    }
                } else {
                    None
                };
                if folded.is_none() {
                    spend_stddev_i128(st);
                    match crate::eval::binop::value_to_bignum(v) {
                        Some(b) => {
                            let sq = b.mul(&b);
                            st.stddev_sum = Some(
                                st.stddev_sum
                                    .as_ref()
                                    .map_or_else(|| b.clone(), |s| s.add(&b)),
                            );
                            st.stddev_sum_sq = Some(
                                st.stddev_sum_sq
                                    .as_ref()
                                    .map_or_else(|| sq.clone(), |s| s.add(&sq)),
                            );
                        }
                        None => st.stddev_saw_float = true,
                    }
                }
            }
            let Some(x) = agg_value_to_f64(v) else {
                return Err(EvalError::TypeMismatch {
                    detail: format!(
                        "{name} needs numeric, got {}",
                        crate::conversions::pg_type_name_for_error_opt(v.data_type())
                    ),
                });
            };
            st.num.count += 1;
            st.num.sum_float += x;
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
                        detail: format!(
                            "{name} needs integer, got {}",
                            crate::conversions::pg_type_name_for_error_opt(other.data_type())
                        ),
                    });
                }
            };
            if matches!(v, Value::BigInt(_)) {
                st.bit_wide = true;
            }
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
            // Counted before the NULL skip: the hypothetical-set
            // fractions divide by the full input size (PG).
            st.within_group_rows += 1;
            if is_null {
                return Ok(());
            }
            st.items.push(v.clone().into_owned());
            if let Some(k) = order_keys {
                st.item_keys.extend(k);
            }
            st.num.count += 1;
        }
        // v7.32 (round-29) — regression family f(Y, X). Only rows with
        // BOTH inputs non-NULL contribute (PG semantics). `v` is Y,
        // `arg2` is X.
        AggKind::Regression => {
            let (Some(y), Some(x)) = (agg_value_to_f64(v), arg2.and_then(agg_value_to_f64)) else {
                return Ok(()); // NULL (or non-numeric) in either input
            };
            // v7.39 (read01 round 115) — accumulate the sums of squared
            // deviations (Sxx / Syy / Sxy) incrementally via the Youngs-Cramer
            // update, matching PG's float8 regression aggregates to the last
            // ULP. The old naive form (`Σx² − (Σx)²/n` at finalize time) is
            // mathematically equal but rounds differently, so `corr` drifted in
            // the 16th digit. reg_sx / reg_sy stay raw sums (for the averages).
            st.reg_n += 1;
            let new_n = st.reg_n as f64;
            let new_sx = st.reg_sx + x;
            let new_sy = st.reg_sy + y;
            if st.reg_n > 1 {
                let n_prev = new_n - 1.0;
                let tmp_x = x * new_n - new_sx;
                let tmp_y = y * new_n - new_sy;
                let scale = 1.0 / (n_prev * new_n);
                st.reg_sxx += tmp_x * tmp_x * scale;
                st.reg_syy += tmp_y * tmp_y * scale;
                st.reg_sxy += tmp_x * tmp_y * scale;
            }
            st.reg_sx = new_sx;
            st.reg_sy = new_sy;
        }
        // v7.32 (round-29) — json_agg / jsonb_agg collect every input
        // (NULL becomes JSON null, per PG) in row order.
        AggKind::JsonAgg => {
            // v7.39 (read01 json.c) — the _strict variants skip NULLs.
            if is_null && name.ends_with("_strict") {
                return Ok(());
            }
            st.items.push(v.clone().into_owned());
            // Attach the ORDER BY key so finalize_synth_rows sorts the
            // elements (`json_agg(x ORDER BY x DESC)`), the same way
            // string_agg / array_agg do.
            if let Some(k) = order_keys {
                st.item_keys.extend(k);
            }
            st.num.count += 1;
        }
        // v7.32 (round-29) — json_object_agg(key, value): keys in
        // `items`, values in `aux_items`. A NULL key is skipped (PG
        // raises; we drop it rather than abort the whole query).
        AggKind::JsonObjectAgg => {
            if is_null {
                return Ok(());
            }
            // v7.39 (read01 json.c) — _strict skips NULL VALUES; _unique
            // raises PG's duplicate-key error.
            let val = arg2.cloned().map(Value::into_owned).unwrap_or(Value::Null);
            if matches!(val, Value::Null) && name.contains("_strict") {
                return Ok(());
            }
            if name.contains("_unique") {
                let kt = match v {
                    Value::Text(s) | Value::Json(s) => s.to_string(),
                    other => crate::json::value_to_json_text(other),
                };
                let dup = st.items.iter().any(|k| match k {
                    Value::Text(s) | Value::Json(s) => *s == kt,
                    other => crate::json::value_to_json_text(other) == kt,
                });
                if dup {
                    return Err(EvalError::TypeMismatch {
                        detail: alloc::format!("duplicate JSON object key value: {kt:?}"),
                    });
                }
            }
            st.items.push(v.clone().into_owned());
            st.aux_items.push(val);
            st.num.count += 1;
        }
    }
    Ok(())
}

#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
pub(crate) fn finalize(name: &str, st: &AggState, mysql: bool) -> Value<'static> {
    match name {
        "count" | "count_star" => Value::BigInt(st.num.count),
        "sum" => {
            if st.num.count == 0 {
                Value::Null
            } else if st.num.use_interval {
                Value::Interval {
                    months: st.num.sum_iv_months as i32,
                    days: st.num.sum_iv_days as i32,
                    micros: st.num.sum_iv_micros as i64,
                    kind: spg_storage::IntervalKind::Finite,
                }
            } else if st.num.use_money {
                Value::Money(st.num.sum_money as i64)
            } else if st.num.use_numeric {
                // v7.38 (read01, T6.P3) — a NaN / ±Infinity input propagates.
                if st.num.sum_num_kind != spg_storage::NumericKind::Finite {
                    Value::numeric_special(st.num.sum_num_kind)
                } else if let Some(big) = &st.num.sum_big {
                    // v7.39 (read01 numeric.c) — the sum spilled past i128;
                    // fold in the int lane and render exactly.
                    let tot = big.add(&spg_storage::bignum::BigNumeric::from_i128(
                        i128::from(st.num.sum_int),
                        0,
                    ));
                    crate::eval::binop::bignum_to_value(tot)
                } else {
                    let (scaled, scale) = crate::numeric::numeric_add(
                        st.num.sum_num_scaled,
                        st.num.sum_num_scale,
                        i128::from(st.num.sum_int),
                        0,
                    );
                    Value::Numeric {
                        scaled,
                        scale,
                        kind: spg_storage::NumericKind::Finite,
                    }
                }
            } else if st.num.use_float {
                let total = st.num.sum_float + (st.num.sum_int as f64);
                // v7.39 (round 269) — sum over REAL input stays real in
                // PG; it widens only when something wider joined the
                // accumulation. avg is deliberately not the same:
                // avg(real) IS double precision (measured on 18.4).
                if st.num.float_not_real {
                    Value::Float(total)
                } else {
                    #[allow(clippy::cast_possible_truncation)]
                    Value::Real(total as f32)
                }
            } else {
                Value::BigInt(st.num.sum_int)
            }
        }
        "avg" => {
            if st.num.count == 0 {
                Value::Null
            } else if st.num.use_interval {
                // PG interval_div: the month quotient truncates and its
                // remainder spills into DAYS (a month = 30 days), taking the
                // whole-day part into the day field and only the sub-day
                // fraction into time; the day remainder then spills into time.
                let n = i128::from(st.num.count);
                let day_us = 86_400_000_000i128;
                let months = i128::from(st.num.sum_iv_months);
                let days = i128::from(st.num.sum_iv_days);
                let month_out = months / n;
                let mrem_days_total = (months % n) * 30; // days (still over n)
                let days_from_month = mrem_days_total / n;
                let mrem_frac_us = (mrem_days_total % n) * day_us / n;
                let day_out = days / n;
                let drem_us = (days % n) * day_us / n;
                let micros = st.num.sum_iv_micros / n + mrem_frac_us + drem_us;
                Value::Interval {
                    months: month_out as i32,
                    days: (day_out + days_from_month) as i32,
                    micros: micros as i64,
                    kind: spg_storage::IntervalKind::Finite,
                }
            } else if st.num.use_money {
                // PG has no avg(money); we accept it as a sensible superset —
                // average of the cent totals, rounded half-away-from-zero.
                //
                // DELIBERATE. Round 664 read "PG refuses, SPG answers" off
                // the F29 list and wrote guards on four accumulators to
                // remove this before a test caught it. Per the round-641
                // policy such a divergence is judged by correctness risk,
                // and this one carries none: money IS cents, so rounding is
                // the type's granularity rather than a loss introduced
                // here, and no PG application can reach the shape, because
                // PG rejects it. Pinned at eight shapes in
                // `e2e_avg_money_round664`.
                let n = i128::from(st.num.count);
                let q =
                    (st.num.sum_money * 2 + if st.num.sum_money >= 0 { n } else { -n }) / (2 * n);
                Value::Money(q as i64)
            } else if st.num.use_numeric {
                // v7.38 (read01, T6.P3) — avg of a special is that special
                // (NaN→NaN, ±Inf→±Inf); PG matches.
                if st.num.sum_num_kind != spg_storage::NumericKind::Finite {
                    Value::numeric_special(st.num.sum_num_kind)
                } else if let Some(big) = &st.num.sum_big {
                    // v7.39 (read01 numeric.c) — bignum avg = spilled sum /
                    // count at PG's division display scale.
                    use spg_storage::bignum::BigNumeric;
                    let sum_tot = big.add(&BigNumeric::from_i128(i128::from(st.num.sum_int), 0));
                    let cnt = BigNumeric::from_i128(i128::from(st.num.count), 0);
                    let rscale = crate::numeric::division_display_scale_big(&sum_tot, &cnt);
                    match sum_tot.div(&cnt, rscale) {
                        Some(q) => crate::eval::binop::bignum_to_value(q),
                        None => Value::Null,
                    }
                } else {
                    let (sum_scaled, sum_scale) = crate::numeric::numeric_add(
                        st.num.sum_num_scaled,
                        st.num.sum_num_scale,
                        i128::from(st.num.sum_int),
                        0,
                    );
                    let (scaled, scale) = crate::numeric::numeric_avg(
                        sum_scaled,
                        sum_scale,
                        i128::from(st.num.count),
                    );
                    Value::Numeric {
                        scaled,
                        scale,
                        kind: spg_storage::NumericKind::Finite,
                    }
                }
            } else if st.num.use_float {
                Value::Float((st.num.sum_float + (st.num.sum_int as f64)) / (st.num.count as f64))
            } else {
                // v7.38 (read01, T4) — avg over integer input is exact NUMERIC
                // (PG: avg(int)/avg(bigint) → numeric), at PG's division display
                // scale. sum(int) is unaffected (it reads sum_int as BigInt).
                let (scaled, scale) = crate::numeric::numeric_avg(
                    i128::from(st.num.sum_int),
                    0,
                    i128::from(st.num.count),
                );
                Value::Numeric {
                    scaled,
                    scale,
                    kind: spg_storage::NumericKind::Finite,
                }
            }
        }
        "min" | "max" | "any_value" => st.extreme.clone().unwrap_or(Value::Null),
        // PG: range_agg over an empty group is NULL; all-empty
        // ranges finalize to the empty multirange {}.
        // v7.39 (round 231) — range_agg collects its inputs verbatim while
        // accumulating; PG's result is a *normalized* multirange, so the
        // spans are sorted, merged where they overlap or abut, and emptied
        // ones dropped exactly once, here. Without this
        // `range_agg` over `[1,3),[5,9),[2,6)` answered all three spans
        // where PG answers the single `{[1,9)}` they cover.
        "range_agg" => match st.extreme.clone() {
            Some(Value::Multirange { kind, ranges }) => Value::Multirange {
                kind,
                ranges: crate::eval::binop::normalize_multirange_spans(kind, &ranges),
            },
            other => other.unwrap_or(Value::Null),
        },
        "range_intersect_agg" => st.extreme.clone().unwrap_or(Value::Null),
        // v7.17.0 — string_agg: join all collected text items with
        // the captured separator. Empty / all-NULL group → NULL
        // (PG semantics).
        "string_agg" | "group_concat" | "xmlagg" => {
            if st.items.is_empty() {
                return Value::Null;
            }
            // group_concat defaults to ',' (MySQL); xmlagg and a
            // separator-less string_agg join bare.
            let sep = st.separator.clone().unwrap_or_else(|| {
                if name == "group_concat" {
                    ",".into()
                } else {
                    String::new()
                }
            });
            // v7.39 (round 762, F31-C2) — per-row separators, when the
            // accumulate path carried them (aligned with items).
            let per_row: Option<&[Option<String>]> =
                if !st.item_seps.is_empty() && st.item_seps.len() == st.items.len() {
                    Some(&st.item_seps)
                } else {
                    None
                };
            let mut out = String::new();
            for (i, item) in st.items.iter().enumerate() {
                if i > 0 {
                    match per_row {
                        Some(seps) => {
                            if let Some(sp) = &seps[i] {
                                out.push_str(sp);
                            }
                        }
                        None => out.push_str(&sep),
                    }
                }
                match item {
                    Value::Text(s) => out.push_str(s),
                    // MySQL group_concat coerces scalars to text;
                    // harmless for string_agg (typed inputs are
                    // Text already).
                    Value::Int(n) => out.push_str(&n.to_string()),
                    Value::BigInt(n) => out.push_str(&n.to_string()),
                    Value::SmallInt(n) => out.push_str(&n.to_string()),
                    Value::Float(f) => out.push_str(&f.to_string()),
                    Value::Bool(b) => {
                        out.push_str(if *b { "1" } else { "0" });
                    }
                    _ => {}
                }
            }
            Value::text(out)
        }
        // v7.17.0 — array_agg: collect into a typed array. NULL
        // elements are preserved per PG. Result type is decided
        // by the first non-NULL element seen (or Text fallback
        // when the whole group is NULL — PG would surface the
        // declared input type, but SPG hasn't yet wired the
        // aggregate's static input-type from `describe`).
        // v7.39 (read01 round 73) — ONE builder, shared with the `ARRAY[…]`
        // literal. This finalize used to dispatch on the first non-NULL element
        // with arms for int and bigint and a text fallback for everything else,
        // so `array_agg(bool_col)` came back as text[] — the same fallback-in-
        // place-of-a-decision that rounds 71/72 dug out of the literal path and
        // the array functions. Fifth site; now there is only one.
        "array_agg" => {
            if st.items.is_empty() {
                return Value::Null;
            }
            crate::eval::values::build_array_from_values(&st.items)
        }
        "bool_and" | "bool_or" => st.bool_acc.map_or(Value::Null, Value::Bool),
        // v7.32 (round-29) — variance / stddev. PG: `variance` ==
        // `var_samp`, `stddev` == `stddev_samp`. samp needs n >= 2
        // (n < 2 → NULL); pop needs n >= 1 (n == 1 → 0).
        "variance" | "var_samp" | "var_pop" | "stddev" | "stddev_samp" | "stddev_pop" => {
            let n = st.num.count;
            if n == 0 {
                return Value::Null;
            }
            let nf = n as f64;
            // v7.39 (round 381) — MySQL's bare STDDEV / VARIANCE are the
            // POPULATION statistics (`STDDEV` = `STDDEV_POP`, `VARIANCE` =
            // `VAR_POP` on MariaDB 11), where PG's bare forms are the
            // SAMPLE ones. `_samp` / `_pop` are explicit and unchanged.
            let pop = name.ends_with("_pop") || (mysql && (name == "stddev" || name == "variance"));
            if !pop && n < 2 {
                // var_samp / stddev (samp) with n == 1 → NULL.
                return Value::Null;
            }
            // v7.38 (read01) — over exact inputs PG's numeric overload applies:
            // variance = (N·Σx² − (Σx)²) / (N² | N·(N−1)) using numeric division's
            // display scale, and stddev is its numeric sqrt. Falls through to the
            // f64 path (a double result, PG's float8 overload) on a float input.
            if !st.stddev_saw_float {
                // v7.39 (round 615) — fold whatever the i128 accumulator holds
                // into the exact pair, once, here.
                if let Some((sum, sum_sq)) = stddev_exact_pair(st) {
                    let (sum, sum_sq) = (&sum, &sum_sq);
                    use spg_storage::bignum::BigNumeric as BN;
                    let nb = BN::from_i128(i128::from(n), 0);
                    let numerator = nb.mul(sum_sq).sub(&sum.mul(sum));
                    let divisor = if pop {
                        nb.mul(&nb)
                    } else {
                        nb.mul(&BN::from_i128(i128::from(n - 1), 0))
                    };
                    // PG returns a bare `0` (scale 0) for a zero / clamped-negative
                    // numerator rather than the division's padded zero.
                    if numerator.is_zero() || numerator.parts().0 {
                        return Value::Numeric {
                            scaled: 0,
                            scale: 0,
                            kind: spg_storage::NumericKind::Finite,
                        };
                    }
                    let rscale = crate::numeric::division_display_scale_big(&numerator, &divisor);
                    if let Some(var) = numerator.div(&divisor, rscale) {
                        let out = if name.starts_with("stddev") {
                            var.sqrt(crate::numeric::sqrt_display_scale_big(&var))
                        } else {
                            Some(var)
                        };
                        if let Some(o) = out {
                            return crate::eval::binop::bignum_to_value(o);
                        }
                    }
                }
            }
            // Match PG's float8 accumulator operation order exactly
            // (utils/adt/float.c float8_var_pop / _samp): the numerator
            // is `N*Σx² - (Σx)²` and the divisor is `N²` (pop) or
            // `N*(N-1)` (samp). SPG previously used the algebraically
            // equal `(Σx² - (Σx)²/N) / denom`, whose different float
            // rounding drifted a ULP from PG on stddev (only masked
            // before by an imprecise hand-rolled sqrt).
            let numerator = (nf * st.sum_sq - st.num.sum_float * st.num.sum_float).max(0.0);
            let divisor = if pop { nf * nf } else { nf * (nf - 1.0) };
            let var = numerator / divisor;
            let result = if name.starts_with("stddev") {
                crate::eval::f64_sqrt(var)
            } else {
                var
            };
            // A float input resolves PG's float8 overload → double precision.
            Value::Float(result)
        }
        // v7.32 (round-29) — bitwise aggregates: None (empty / all-NULL)
        // → SQL NULL.
        "bit_and" | "bit_or" | "bit_xor" => st.bit_acc.map_or(Value::Null, |acc| {
            if st.bit_wide {
                Value::BigInt(acc)
            } else {
                Value::Int(acc as i32)
            }
        }),
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
            // v7.39 (read01 round 115) — Sxx / Syy / Sxy are now the
            // Youngs-Cramer running deviation sums (accumulated above), so they
            // are used directly rather than re-derived from the raw squares.
            let sxx = st.reg_sxx;
            let syy = st.reg_syy;
            let sxy = st.reg_sxy;
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
        "json_agg" | "jsonb_agg" | "json_arrayagg" | "json_agg_strict" | "jsonb_agg_strict" => {
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
            // jsonb_agg yields canonical jsonb (nested object keys sorted,
            // numbers normalised); json_agg keeps the input verbatim.
            let result = Value::json(out);
            if name.starts_with("jsonb_agg") {
                crate::json::canonicalize_value(result)
            } else {
                result
            }
        }
        // v7.32 (round-29) — json_object_agg: a JSON object built from
        // the parallel key (`items`) / value (`aux_items`) streams.
        "json_object_agg"
        | "jsonb_object_agg"
        | "json_objectagg"
        | "json_object_agg_strict"
        | "jsonb_object_agg_strict"
        | "json_object_agg_unique"
        | "jsonb_object_agg_unique"
        | "json_object_agg_unique_strict"
        | "jsonb_object_agg_unique_strict" => {
            if st.items.is_empty() {
                return Value::Null;
            }
            // Object keys are always JSON strings (PG coerces).
            let key_text = |key: &Value| -> String {
                match key {
                    Value::Text(s) | Value::Json(s) => s.to_string(),
                    other => crate::json::value_to_json_text(other),
                }
            };
            // jsonb dedups keys keeping the last value (jsonb is a
            // map); json preserves every pair including duplicates.
            let dedup = name.starts_with("jsonb_object_agg");
            // (key, value-index) pairs in first-seen key order; for
            // jsonb a repeated key updates its value-index in place.
            let mut pairs: Vec<(String, usize)> = Vec::with_capacity(st.items.len());
            for (i, key) in st.items.iter().enumerate() {
                let kt = key_text(key);
                if dedup {
                    if let Some(slot) = pairs.iter_mut().find(|(k, _)| *k == kt) {
                        slot.1 = i;
                        continue;
                    }
                }
                pairs.push((kt, i));
            }
            // v7.39 (read01 json.c) — PG's json_object_agg emits the
            // distinctive "{ \"k\" : v, ... }" spacing (jsonb variants
            // canonicalize it away below).
            let mut out = String::from("{ ");
            for (n, (kt, i)) in pairs.iter().enumerate() {
                if n > 0 {
                    out.push_str(", ");
                }
                out.push_str(&crate::json::value_to_json_text(&Value::text(kt.clone())));
                out.push_str(" : ");
                let val = st.aux_items.get(*i).unwrap_or(&Value::Null);
                out.push_str(&crate::json::value_to_json_text(val));
            }
            out.push_str(" }");
            // jsonb_object_agg emits canonical jsonb — keys sorted by PG's
            // (length, byte) order; json_object_agg keeps first-seen order.
            let result = Value::json(out);
            if dedup {
                crate::json::canonicalize_value(result)
            } else {
                result
            }
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
        Value::Real(x) => Some(f64::from(*x)),
        Value::Numeric { scaled, scale, .. } => Some(numeric_to_f64(*scaled, *scale)),
        _ => None,
    }
}

/// The array form of a `percentile_cont/disc` direct argument
/// (`percentile_cont(ARRAY[0.25,0.5,0.75])`), as f64 fractions. `None` when the
/// direct argument is a plain scalar fraction. A NULL element stays `None` —
/// PG yields a NULL result element for it.
fn percentile_fraction_array(v: Option<&Value>) -> Option<Vec<Option<f64>>> {
    match v? {
        Value::FloatArray(a) => Some(a.clone()),
        Value::NumericArray(a) => Some(
            a.iter()
                .map(|x| x.map(|(scaled, scale)| numeric_to_f64(scaled, scale)))
                .collect(),
        ),
        Value::IntArray(a) => Some(a.iter().map(|x| x.map(f64::from)).collect()),
        // Array literals (`ARRAY[0.25,0.5,0.75]`) evaluate to a TextArray of the
        // element renderings; parse each back to f64.
        Value::TextArray(a) => Some(
            a.iter()
                .map(|x| x.as_deref().and_then(|s| s.parse::<f64>().ok()))
                .collect(),
        ),
        _ => None,
    }
}

/// Build an array Value from a list of scalar values, dispatching on the first
/// non-NULL element's type (mirrors array_agg's finalize). Used by the array
/// form of `percentile_disc`, whose result is an array of the ordered-column
/// element type.
fn values_to_array(picked: &[Value<'_>]) -> Value<'static> {
    let owned: alloc::vec::Vec<Value<'static>> =
        picked.iter().map(|v| v.clone().into_owned()).collect();
    crate::eval::values::build_array_from_values(&owned)
}

/// NUMERIC → f64 for the float-math aggregates (stddev / variance / corr /
/// percentile_cont). `scaled × 10^-scale`; `10^scale` fits in i128 for the
/// NUMERIC scale range, so no `f64::powi` (unavailable under no_std) is needed.
#[allow(clippy::cast_precision_loss)]
fn numeric_to_f64(scaled: i128, scale: u16) -> f64 {
    (scaled as f64) / (10i128.pow(u32::from(scale)) as f64)
}

/// v7.32 (round-29) — finalize a WITHIN GROUP aggregate. `st.items` is
/// already sorted by the `WITHIN GROUP (ORDER BY …)` spec. `direct` is
/// the evaluated direct argument: the fraction for `percentile_*`, the
/// first hypothetical value for the hypothetical-set family (`rank`
/// etc. — `direct_extra` carries the rest of a multi-key call), and
/// unused by `mode`. `order_by` is the sort spec; the hypothetical-set
/// family compares in the sort direction (multi-key via `st.item_keys`).
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]
fn finalize_ordered_set(
    name: &str,
    st: &AggState,
    direct: Option<&Value>,
    direct_extra: &[Value<'static>],
    order_by: &[spg_sql::ast::OrderBy],
    order_collations: &[Option<alloc::string::String>],
    mysql: bool,
) -> Result<Value<'static>, EvalError> {
    let fraction = direct;
    // v7.39 (read01 orderedsetaggs.c) — PG validates the percentile
    // fraction before looking at the rows (an out-of-range fraction
    // errors even over an empty group), and a NULL fraction is NULL.
    let check_fraction = |f: f64| -> Result<f64, EvalError> {
        if !(0.0..=1.0).contains(&f) || f.is_nan() {
            return Err(EvalError::TypeMismatch {
                detail: format!("percentile value {f} is not between 0 and 1"),
            });
        }
        Ok(f)
    };
    let scalar_fraction: Option<Result<f64, EvalError>> =
        if matches!(name, "percentile_cont" | "percentile_disc") {
            match fraction {
                None | Some(Value::Null) => return Ok(Value::Null),
                Some(v) => match percentile_fraction_array(Some(v)) {
                    Some(fracs) => {
                        for f in fracs.iter().flatten() {
                            check_fraction(*f)?;
                        }
                        None
                    }
                    None => Some(
                        agg_value_to_f64(v)
                            .ok_or_else(|| EvalError::TypeMismatch {
                                detail: format!(
                                    "percentile fraction must be numeric, got {}",
                                    crate::conversions::pg_type_name_for_error_opt(v.data_type())
                                ),
                            })
                            .and_then(check_fraction),
                    ),
                },
            }
        } else {
            None
        };
    let items = &st.items;
    if items.is_empty() {
        // A hypothetical row ranks first over an empty group; the
        // distribution functions are 0 / divide-by-(n+1).
        return Ok(match name {
            "rank" | "dense_rank" => Value::BigInt(1),
            "percent_rank" => Value::Float(0.0),
            "cume_dist" => Value::Float(1.0),
            _ => Value::Null,
        });
    }
    let n = items.len();
    Ok(match name {
        // v7.32 (round-29) — hypothetical-set: the rank the direct value
        // would have if inserted into the group, in the sort direction.
        "rank" | "dense_rank" | "percent_rank" | "cume_dist" => {
            let Some(h) = fraction else {
                return Ok(Value::Null);
            };
            // v7.39 (read01 orderedsetaggs.c) — the multi-key form
            // compares the hypothetical tuple against the collected
            // `item_keys` tuples with the full sort spec.
            let kw = order_by.len();
            let multi = kw > 1 && st.item_keys.len() == items.len() * kw;
            let hv: Vec<Value<'static>> = core::iter::once(h.clone().into_owned())
                .chain(direct_extra.iter().cloned())
                .collect();
            let (desc, nulls_first) = order_by
                .first()
                .map_or((false, None), |o| (o.desc, o.nulls_first));
            let cmp_i = |i: usize| -> core::cmp::Ordering {
                if multi {
                    cmp_order_keys(
                        order_by,
                        &[],
                        order_collations,
                        &st.item_keys[i * kw..(i + 1) * kw],
                        &hv,
                        mysql,
                    )
                } else {
                    crate::order_by_value_cmp_in(desc, nulls_first, &items[i], h, mysql)
                }
            };
            let mut before: Vec<usize> = Vec::new(); // sort strictly before h
            let mut before_or_eq = 0usize; // sort before-or-peer with h
            for i in 0..n {
                match cmp_i(i) {
                    core::cmp::Ordering::Less => {
                        before.push(i);
                        before_or_eq += 1;
                    }
                    core::cmp::Ordering::Equal => before_or_eq += 1,
                    core::cmp::Ordering::Greater => {}
                }
            }
            // PG divides by the FULL input size (NULL rows included);
            // `n` counts only the non-NULL values `items` holds.
            let nn = st.within_group_rows.max(n) as f64;
            match name {
                "rank" => Value::BigInt((before.len() + 1) as i64),
                "dense_rank" => {
                    // Count distinct sort-key tuples among the strictly-
                    // before rows (items arrive unsorted relative to
                    // item_keys in the multi-key form, so sort + dedup).
                    let tuple_cmp = |&x: &usize, &y: &usize| -> core::cmp::Ordering {
                        if multi {
                            cmp_order_keys(
                                order_by,
                                &[],
                                order_collations,
                                &st.item_keys[x * kw..(x + 1) * kw],
                                &st.item_keys[y * kw..(y + 1) * kw],
                                mysql,
                            )
                        } else {
                            value_cmp(&items[x], &items[y])
                        }
                    };
                    let mut sorted = before.clone();
                    sorted.sort_by(tuple_cmp);
                    let mut distinct = 0usize;
                    for (k, &i) in sorted.iter().enumerate() {
                        if k == 0 || tuple_cmp(&sorted[k - 1], &i) != core::cmp::Ordering::Equal {
                            distinct += 1;
                        }
                    }
                    Value::BigInt((distinct + 1) as i64)
                }
                "percent_rank" => Value::Float(before.len() as f64 / nn),
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
        // The first value whose cumulative fraction reaches `f`. PG accepts
        // both a scalar fraction (→ the element) and an array of fractions (→
        // an array of the ordered-column element type, with NULL fractions
        // yielding NULL elements).
        "percentile_disc" => {
            let idx_at = |f: f64| -> usize {
                if f <= 0.0 {
                    0
                } else {
                    (crate::eval::f64_ceil(f * n as f64) as usize)
                        .saturating_sub(1)
                        .min(n - 1)
                }
            };
            if let Some(fracs) = percentile_fraction_array(fraction) {
                let picked: Vec<Value> = fracs
                    .iter()
                    .map(|f| f.map_or(Value::Null, |f| items[idx_at(f)].clone()))
                    .collect();
                return Ok(values_to_array(&picked));
            }
            let f = scalar_fraction.transpose()?.unwrap_or(0.0);
            items[idx_at(f)].clone()
        }
        // Linear interpolation between the two bracketing values. PG accepts
        // both a scalar fraction (→ float) and an array of fractions (→ a
        // float array, one interpolated value per requested percentile).
        "percentile_cont" => {
            // v7.39 (read01 orderedsetaggs.c) — the INTERVAL overload
            // interpolates component-wise with PG's month→day→time
            // remainder spill (a month is 30 days, a day 86400 s).
            if items.iter().all(|v| matches!(v, Value::Interval { .. })) {
                let iv = |i: usize| -> (f64, f64, f64) {
                    match &items[i] {
                        Value::Interval {
                            months,
                            days,
                            micros,
                            kind,
                        } => (f64::from(*months), f64::from(*days), *micros as f64),
                        _ => unreachable!(),
                    }
                };
                let at = |f: f64| -> Value<'static> {
                    if n == 1 {
                        return items[0].clone();
                    }
                    let rank = f * (n as f64 - 1.0);
                    let lo = crate::eval::f64_floor(rank) as usize;
                    let hi = crate::eval::f64_ceil(rank) as usize;
                    let frac = rank - lo as f64;
                    let (lm, ld, lu) = iv(lo);
                    let (hm, hd, hu) = iv(hi);
                    let dm = (hm - lm) * frac;
                    let m_i = dm as i64; // trunc toward zero
                    let rem_days = (dm - m_i as f64) * 30.0 + (hd - ld) * frac;
                    let d_i = rem_days as i64;
                    let us = (rem_days - d_i as f64) * 86_400_000_000.0 + (hu - lu) * frac;
                    Value::Interval {
                        months: (lm as i64 + m_i) as i32,
                        days: (ld as i64 + d_i) as i32,
                        micros: lu as i64 + libm::round(us) as i64,
                        kind: spg_storage::IntervalKind::Finite,
                    }
                };
                if let Some(fracs) = percentile_fraction_array(fraction) {
                    let picked: Vec<Value> =
                        fracs.iter().map(|f| f.map_or(Value::Null, at)).collect();
                    return Ok(values_to_array(&picked));
                }
                let f = scalar_fraction.transpose()?.unwrap_or(0.0);
                return Ok(at(f));
            }
            let Some(nums) = items
                .iter()
                .map(agg_value_to_f64)
                .collect::<Option<Vec<f64>>>()
            else {
                return Ok(Value::Null); // non-numeric ordered set
            };
            let at = |f: f64| -> f64 {
                if n == 1 {
                    return nums[0];
                }
                let rank = f * (n as f64 - 1.0);
                let lo = crate::eval::f64_floor(rank) as usize;
                let hi = crate::eval::f64_ceil(rank) as usize;
                let frac = rank - lo as f64;
                nums[lo] + (nums[hi] - nums[lo]) * frac
            };
            if let Some(fracs) = percentile_fraction_array(fraction) {
                return Ok(Value::FloatArray(fracs.iter().map(|f| f.map(at)).collect()));
            }
            let f = scalar_fraction.transpose()?.unwrap_or(0.0);
            Value::Float(at(f))
        }
        _ => unreachable!(),
    })
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
        // v7.38 (read01, T4) — sum(int) → bigint, sum(bigint) → numeric (PG
        // widens to numeric to defend against i64 overflow), sum(float) → float.
        "sum" => match arg_ty {
            Some(DataType::Float) => DataType::Float,
            Some(DataType::BigInt) => DataType::Numeric {
                precision: 0,
                scale: 0,
            },
            _ => DataType::BigInt,
        },
        // v7.38 (read01, T4) — avg over any integer / numeric input is NUMERIC
        // (PG); only avg(float8) stays double precision.
        "avg" => match arg_ty {
            Some(DataType::Float) => DataType::Float,
            _ => DataType::Numeric {
                precision: 0,
                scale: 0,
            },
        },
        // v7.17.0 — string_agg always returns TEXT.
        "string_agg" | "group_concat" | "xmlagg" => DataType::Text,
        // v7.39 (read01 round 73) — the STATIC type follows the same rule the
        // finalize does, so `pg_typeof(array_agg(b))` is `boolean[]`.
        "array_agg" => match arg_ty {
            Some(DataType::Int | DataType::SmallInt) => DataType::IntArray,
            Some(DataType::BigInt) => DataType::BigIntArray,
            Some(DataType::Bool) => DataType::BoolArray,
            Some(DataType::Date) => DataType::DateArray,
            Some(DataType::Timestamp) => DataType::TimestampArray,
            Some(DataType::Timestamptz) => DataType::TimestamptzArray,
            Some(DataType::Uuid) => DataType::UuidArray,
            Some(DataType::Float) => DataType::FloatArray,
            Some(DataType::Numeric { .. }) => DataType::NumericArray,
            Some(DataType::Bytes) => DataType::BytesArray,
            _ => DataType::TextArray,
        },
        // v7.17.0 — boolean aggregates always return BOOL (nullable
        // — empty / all-NULL group → NULL).
        "bool_and" | "bool_or" => DataType::Bool,
        // v7.32 (round-29) — variance / stddev are floating point;
        // percentile_cont interpolates to float; the regression family
        // (except regr_count) is floating point.
        // v7.38 (read01, T4.3) — PG stddev / variance return NUMERIC.
        "stddev" | "stddev_samp" | "stddev_pop" | "variance" | "var_samp" | "var_pop" => {
            DataType::Numeric {
                precision: 0,
                scale: 0,
            }
        }
        "percentile_cont" | "covar_pop" | "covar_samp" | "corr" | "regr_avgx" | "regr_avgy"
        | "regr_slope" | "regr_intercept" | "regr_r2" | "regr_sxx" | "regr_syy" | "regr_sxy" => {
            DataType::Float
        }
        // v7.32 (round-29) — bitwise aggregates, regr_count, and the
        // integer hypothetical-set ranks return an integer.
        // v7.38 (read01, T4.4) — bit_and/or/xor return the INPUT integer type
        // (PG: bit_and(int) → integer, bit_and(bigint) → bigint).
        "bit_and" | "bit_or" | "bit_xor" => match arg_ty {
            Some(DataType::SmallInt) => DataType::SmallInt,
            Some(DataType::BigInt) => DataType::BigInt,
            _ => DataType::Int,
        },
        "regr_count" | "rank" | "dense_rank" => DataType::BigInt,
        // v7.32 (round-29) — hypothetical-set distribution functions.
        "percent_rank" | "cume_dist" => DataType::Float,
        // v7.32 (round-29) — JSON aggregates return JSON.
        "json_agg" | "jsonb_agg" | "json_object_agg" | "jsonb_object_agg" | "json_arrayagg"
        | "json_objectagg" => DataType::Json,
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

/// v7.39 (round 620) — PG's strict GROUP BY rule, and the diagnosis it earns.
///
/// `SELECT id, count(*) FROM dc` answered `column "id" does not exist`. The
/// column plainly exists; what it is not is grouped. The message came out that
/// way because there was no rule at all — the grouped row carries only the
/// grouping keys and the aggregates, so the reference simply failed to resolve
/// at evaluation time, and the resolver said the only thing it knew. A user
/// reading it goes looking for a typo or a missing table.
///
/// Returns the first bare column reference that is a real input column, is not
/// covered by a grouping expression, and is not inside an aggregate. Variants
/// this walker does not descend into are left alone, so an uncovered nesting
/// keeps the old behaviour rather than inventing an error: under-reporting is
/// the status quo, over-reporting would break queries that run today.
fn first_ungrouped_column<'a>(
    e: &'a Expr,
    group_exprs: &[Expr],
    columns: &[ColumnSchema],
    licensed: &[alloc::string::String],
) -> Option<&'a spg_sql::ast::ColumnName> {
    if group_exprs.iter().any(|g| g == e) {
        return None;
    }
    let rec = |x: &'a Expr| first_ungrouped_column(x, group_exprs, columns, licensed);
    match e {
        Expr::Column(c) => {
            (column_ref_is_input(c, columns) && !column_is_key_determined(c, licensed)).then_some(c)
        }
        // An aggregate's arguments are exactly what does not need grouping.
        Expr::FunctionCall { name, .. } if is_aggregate_name(&name.to_ascii_lowercase()) => None,
        Expr::AggregateOrdered { .. } => None,
        // A subquery carries its own scope and its own rules.
        Expr::ScalarSubquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => None,
        Expr::FunctionCall { args, .. } => args.iter().find_map(rec),
        Expr::Binary { lhs, rhs, .. } => rec(lhs).or_else(|| rec(rhs)),
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::BoolTest { expr, .. } => rec(expr),
        Expr::Like { expr, pattern, .. } => rec(expr).or_else(|| rec(pattern)),
        Expr::InList { expr, list, .. } => rec(expr).or_else(|| list.iter().find_map(rec)),
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => operand
            .as_deref()
            .and_then(rec)
            .or_else(|| branches.iter().find_map(|(w, t)| rec(w).or_else(|| rec(t))))
            .or_else(|| else_branch.as_deref().and_then(rec)),
        _ => None,
    }
}

/// v7.39 (round 620) — does this column reference name an INPUT column?
///
/// A joined schema names its columns `a.s`; a single-table one names them `s`
/// and answers to the active alias. Matching only the bare name — which the
/// first cut of round 620 did — makes every qualified reference in a join
/// invisible to both the check and the rewrite below, which is how they
/// reached evaluation and came back `missing FROM-clause entry for table "a"`.
fn column_ref_is_input(c: &spg_sql::ast::ColumnName, columns: &[ColumnSchema]) -> bool {
    if let Some(q) = &c.qualifier {
        let composite = alloc::format!("{q}.{}", c.name);
        if columns
            .iter()
            .any(|col| col.name.eq_ignore_ascii_case(&composite))
        {
            return true;
        }
    }
    columns
        .iter()
        .any(|col| col.name.eq_ignore_ascii_case(&c.name))
}

/// v7.39 (round 620) — the qualifiers whose PRIMARY KEY is wholly present in
/// the GROUP BY list, which licenses every OTHER column of those tables.
///
/// `SELECT s, count(*) FROM dc GROUP BY id` where `id` is the primary key is
/// answered by PG and was REFUSED here — a query that runs on PG and fails on
/// SPG, which is worse than any wording. One row per `id` means `s` has
/// exactly one value in the group, so there is nothing ambiguous to resolve;
/// the rule is the SQL standard's functional dependency, and PG applies it for
/// a base table's primary key.
///
/// Every FROM entry is considered separately, so a join licenses the side
/// whose key is grouped and not the other: `SELECT a.s, b.t … JOIN … GROUP BY
/// a.id` answers `a.s` and still refuses `b.t`, which is what PG does.
///
/// The empty string stands for the unqualified single-table case.
fn qualifiers_grouped_by_primary_key(
    stmt: &SelectStatement,
    group_exprs: &[Expr],
    columns: &[ColumnSchema],
    catalog: Option<&spg_storage::Catalog>,
) -> Vec<alloc::string::String> {
    let (Some(from), Some(cat)) = (stmt.from.as_ref(), catalog) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let refs = core::iter::once(&from.primary).chain(from.joins.iter().map(|j| &j.table));
    let single = from.joins.is_empty();
    for tr in refs {
        if tr.unnest_expr.is_some() {
            continue;
        }
        let Some(table) = cat.get(&tr.name) else {
            continue;
        };
        let schema = table.schema();
        let Some(pk) = schema
            .uniqueness_constraints
            .iter()
            .find(|u| u.is_primary_key && !u.columns.is_empty())
        else {
            continue;
        };
        let qual = tr.alias.as_deref().unwrap_or(tr.name.as_str());
        let all_keys_grouped = pk.columns.iter().all(|&pos| {
            let Some(name) = schema.columns.get(pos).map(|c| &c.name) else {
                return false;
            };
            // The key column has to be grouped by AS ITSELF, and as this
            // table's: an unqualified spelling only counts when there is one
            // table for it to mean.
            group_exprs.iter().any(|g| match g {
                Expr::Column(c) if c.name.eq_ignore_ascii_case(name) => {
                    let belongs = match &c.qualifier {
                        Some(q) => q.eq_ignore_ascii_case(qual),
                        None => single,
                    };
                    belongs && column_ref_is_input(c, columns)
                }
                _ => false,
            })
        });
        if all_keys_grouped {
            out.push(alloc::string::String::from(qual));
            if single {
                out.push(alloc::string::String::new());
            }
        }
    }
    out
}

/// True when this column reference is licensed by one of those keys.
fn column_is_key_determined(
    c: &spg_sql::ast::ColumnName,
    licensed: &[alloc::string::String],
) -> bool {
    let q = c.qualifier.as_deref().unwrap_or("");
    licensed.iter().any(|l| l.eq_ignore_ascii_case(q))
}

/// v7.39 (round 405) — MySQL's loose GROUP BY: a non-aggregated column
/// that is not in GROUP BY is allowed and reads any (the first-seen) row's
/// value in the group. PG (and SPG until now) rejects it. Wrapping such a
/// bare column in `any_value(col)` reuses the existing aggregate machinery.
/// A whole grouping expression stays as-is; an aggregate call is not
/// descended into (its inner columns are already fine); a non-aggregate
/// function's argument columns are wrapped individually
/// (`UPPER(name)` → `UPPER(any_value(name))`).
fn wrap_loose_group_columns(
    e: Expr,
    group_exprs: &[Expr],
    columns: &[ColumnSchema],
    // v7.39 (round 620) — `None` wraps every ungrouped column, which is what
    // MySQL's loose GROUP BY means. `Some(quals)` wraps only the columns a
    // grouped primary key determines, so a join licenses the side whose key is
    // grouped and leaves the other to be refused.
    licensed: Option<&[alloc::string::String]>,
) -> Expr {
    if group_exprs.iter().any(|g| *g == e) {
        return e;
    }
    let wrap = |x: Expr| wrap_loose_group_columns(x, group_exprs, columns, licensed);
    match e {
        Expr::Column(c) => {
            let claimed = column_ref_is_input(&c, columns)
                && licensed.is_none_or(|l| column_is_key_determined(&c, l));
            if claimed {
                Expr::FunctionCall {
                    name: String::from("any_value"),
                    args: alloc::vec![Expr::Column(c)],
                }
            } else {
                Expr::Column(c)
            }
        }
        Expr::FunctionCall { name, args } if is_aggregate_name(&name.to_ascii_lowercase()) => {
            Expr::FunctionCall { name, args }
        }
        Expr::AggregateOrdered { .. } => e,
        Expr::FunctionCall { name, args } => Expr::FunctionCall {
            name,
            args: args.into_iter().map(wrap).collect(),
        },
        Expr::Binary { op, lhs, rhs } => Expr::Binary {
            op,
            lhs: Box::new(wrap(*lhs)),
            rhs: Box::new(wrap(*rhs)),
        },
        Expr::Unary { op, expr } => Expr::Unary {
            op,
            expr: Box::new(wrap(*expr)),
        },
        Expr::Cast { expr, target } => Expr::Cast {
            expr: Box::new(wrap(*expr)),
            target,
        },
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: Box::new(wrap(*expr)),
            negated,
        },
        Expr::BoolTest {
            expr,
            value,
            negated,
        } => Expr::BoolTest {
            expr: Box::new(wrap(*expr)),
            value,
            negated,
        },
        Expr::Like {
            expr,
            pattern,
            negated,
            case_insensitive,
        } => Expr::Like {
            expr: Box::new(wrap(*expr)),
            pattern: Box::new(wrap(*pattern)),
            negated,
            case_insensitive,
        },
        Expr::InList {
            expr,
            list,
            negated,
        } => Expr::InList {
            expr: Box::new(wrap(*expr)),
            list: list.into_iter().map(wrap).collect(),
            negated,
        },
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => Expr::Case {
            operand: operand.map(|o| Box::new(wrap(*o))),
            branches: branches
                .into_iter()
                .map(|(w, t)| (wrap(w), wrap(t)))
                .collect(),
            else_branch: else_branch.map(|b| Box::new(wrap(*b))),
        },
        other => other,
    }
}

/// v7.39 (round 404) — MySQL lets HAVING (and ORDER BY) reference a
/// SELECT-list alias (`SELECT g, SUM(v) AS sv … HAVING sv > 30`); PG does
/// not. Before the aggregate rewrite, replace a bare `Column(alias)` with
/// the SELECT expression it names, so the aggregate rewrite then maps it to
/// its synthetic column. A nesting this walker does not cover simply leaves
/// the column unresolved (the pre-existing "column does not exist" error),
/// never a wrong result.
fn substitute_having_aliases(e: Expr, aliases: &[(String, Expr)]) -> Expr {
    use spg_sql::ast::ColumnName;
    let sub = |x: Expr| substitute_having_aliases(x, aliases);
    match e {
        Expr::Column(ColumnName {
            qualifier: None,
            name,
        }) => aliases
            .iter()
            .find(|(a, _)| a.eq_ignore_ascii_case(&name))
            .map_or_else(
                || {
                    Expr::Column(ColumnName {
                        qualifier: None,
                        name,
                    })
                },
                |(_, expr)| expr.clone(),
            ),
        Expr::Binary { op, lhs, rhs } => Expr::Binary {
            op,
            lhs: Box::new(sub(*lhs)),
            rhs: Box::new(sub(*rhs)),
        },
        Expr::Unary { op, expr } => Expr::Unary {
            op,
            expr: Box::new(sub(*expr)),
        },
        Expr::FunctionCall { name, args } => Expr::FunctionCall {
            name,
            args: args.into_iter().map(sub).collect(),
        },
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: Box::new(sub(*expr)),
            negated,
        },
        Expr::BoolTest {
            expr,
            value,
            negated,
        } => Expr::BoolTest {
            expr: Box::new(sub(*expr)),
            value,
            negated,
        },
        Expr::Like {
            expr,
            pattern,
            negated,
            case_insensitive,
        } => Expr::Like {
            expr: Box::new(sub(*expr)),
            pattern: Box::new(sub(*pattern)),
            negated,
            case_insensitive,
        },
        Expr::InList {
            expr,
            list,
            negated,
        } => Expr::InList {
            expr: Box::new(sub(*expr)),
            list: list.into_iter().map(sub).collect(),
            negated,
        },
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => Expr::Case {
            operand: operand.map(|o| Box::new(sub(*o))),
            branches: branches
                .into_iter()
                .map(|(w, t)| (sub(w), sub(t)))
                .collect(),
            else_branch: else_branch.map(|b| Box::new(sub(*b))),
        },
        Expr::Cast { expr, target } => Expr::Cast {
            expr: Box::new(sub(*expr)),
            target,
        },
        other => other,
    }
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
        Expr::NamedArg { name, expr } => Expr::NamedArg {
            name: name.clone(),
            expr: alloc::boxed::Box::new(rewrite_expr(expr, group_exprs, aggs)),
        },
        Expr::Variadic(expr) => Expr::Variadic(alloc::boxed::Box::new(rewrite_expr(
            expr,
            group_exprs,
            aggs,
        ))),
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
                    collation: o.collation.clone(),
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
        Expr::FieldAccess { base, field } => Expr::FieldAccess {
            base: Box::new(rewrite_expr(base, group_exprs, aggs)),
            field: field.clone(),
        },
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: Box::new(rewrite_expr(expr, group_exprs, aggs)),
            negated: *negated,
        },
        Expr::BoolTest {
            expr,
            value,
            negated,
        } => Expr::BoolTest {
            expr: Box::new(rewrite_expr(expr, group_exprs, aggs)),
            value: *value,
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
            field: field.clone(),
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
        Expr::RowInSubquery {
            row,
            subquery,
            negated,
        } => Expr::RowInSubquery {
            row: row
                .iter()
                .map(|el| rewrite_expr(el, group_exprs, aggs))
                .collect(),
            subquery: Box::new(rewrite_group_keys_in_select(subquery, group_exprs)),
            negated: *negated,
        },
        Expr::RowCmpSubquery { row, op, subquery } => Expr::RowCmpSubquery {
            row: row
                .iter()
                .map(|el| rewrite_expr(el, group_exprs, aggs))
                .collect(),
            op: *op,
            subquery: Box::new(rewrite_group_keys_in_select(subquery, group_exprs)),
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
        Expr::ArraySlice { target, lo, hi } => Expr::ArraySlice {
            target: Box::new(rewrite_expr(target, group_exprs, aggs)),
            lo: lo
                .as_ref()
                .map(|b| Box::new(rewrite_expr(b, group_exprs, aggs))),
            hi: hi
                .as_ref()
                .map(|b| Box::new(rewrite_expr(b, group_exprs, aggs))),
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
    encode_one_in(out, v, false);
}

/// v7.39 (round 364, M4 P2) — key encoder with the session dialect. On a
/// MySQL session a text group / distinct key is FOLDED (accent- and
/// case-insensitive) so `Foo`/`foo`/`FOO` share one group and `bar`/`Bär`
/// merge — while the group's OUTPUT value stays the first row's original,
/// because only the key is folded, not the stored value.
fn encode_one_in(out: &mut String, v: &Value, mysql: bool) {
    use core::fmt::Write;
    if mysql {
        if let Value::Text(s) | Value::Json(s) = v {
            let _ = write!(out, "S{}|", spg_storage::mysql_compare_fold(s));
            return;
        }
        if let Value::BpChar(s) = v {
            let folded = spg_storage::mysql_compare_fold_char(s);
            let _ = write!(out, "S{folded}|");
            return;
        }
    }
    encode_one_raw(out, v);
}

fn encode_one_raw(out: &mut String, v: &Value) {
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
            // v7.37.16 — fold -0.0 into 0.0: PG's float8 equality (hash and
            // btree opclasses) treats them as one value, so GROUP BY /
            // DISTINCT must key them together (count(DISTINCT) differential).
            // NaN needs no fold — every NaN renders "NaN" here already.
            let x = if *x == 0.0 { 0.0 } else { *x };
            let _ = write!(out, "F{x}|");
        }
        Value::Real(x) => {
            let x = if *x == 0.0 { 0.0 } else { *x };
            let _ = write!(out, "R{x}|");
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
        // v7.38 (read01, T11/R3) — bpchar groups / dedups blank-insensitively,
        // and shares the text key so `'ab'::char(4)` and `'ab'` co-group.
        Value::BpChar(s) => {
            out.push('S');
            out.push_str(s.trim_end_matches(' '));
            out.push('|');
        }
        Value::Vector(v) => {
            out.push('V');
            for x in v.iter() {
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
        Value::Numeric { scaled, scale, .. } => {
            // v7.38 (read01) — DISTINCT keys numerically-equal decimals as one
            // regardless of scale (1.0 = 1.00), so strip trailing fractional
            // zeros before encoding, matching PG (and set-op / GROUP BY dedup).
            let (mut s, mut sc) = (*scaled, *scale);
            while sc > 0 && s % 10 == 0 {
                s /= 10;
                sc -= 1;
            }
            out.push('D');
            out.push_str(&s.to_string());
            out.push('@');
            out.push_str(&sc.to_string());
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
            kind,
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
/// materialising an owned Vec<Value<'static>> first.
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
/// v7.39 (round 590) — append ONE value's encoding, for the join key that
/// mixes stored cells with computed ones and so cannot clear as it goes.
/// v7.39 (round 590, moved here round 593+) — one component of a key with a COMPUTED side.
///
/// The whole requirement is that two values SQL calls equal encode the same,
/// or the join silently loses rows. Across the numeric family that is not
/// free: `5` as INT, `5` as BIGINT, `5.0` as double and `5.00` as NUMERIC all
/// compare equal and would otherwise carry four different tags, so they are
/// all rendered as one canonical decimal. A non-integral value can never
/// equal an integer, so it simply renders as itself; NaN equals nothing and
/// any encoding will do. Everything outside the numeric family keeps the
/// encoder the column-to-column path already uses.
pub(crate) fn push_canonical_key(out: &mut String, v: &Value) {
    use core::fmt::Write;
    match v {
        Value::SmallInt(n) => {
            let _ = write!(out, "n{n}|");
        }
        Value::Int(n) => {
            let _ = write!(out, "n{n}|");
        }
        Value::BigInt(n) => {
            let _ = write!(out, "n{n}|");
        }
        // `-0.0` prints with its sign but equals `0`.
        Value::Float(f) if *f == 0.0 => out.push_str("n0|"),
        Value::Float(f) => {
            let _ = write!(out, "n{f}|");
        }
        Value::Numeric { .. } => {
            let t = crate::eval::value_to_text(v);
            let t = if t.contains('.') {
                t.trim_end_matches('0').trim_end_matches('.')
            } else {
                t.as_str()
            };
            let _ = write!(out, "n{t}|");
        }
        _ => encode_one_into(out, v),
    }
}

/// v7.39 (round 596) — a whole key encoded the canonical way, for the two
/// sides of a decorrelated EXISTS: the set is built from the inner column's
/// values and probed with the outer EXPRESSION's, and those need not share a
/// numeric width for `=` to call them equal.
pub(crate) fn encode_canonical_key(vals: &[Value<'_>]) -> String {
    let mut out = String::new();
    for v in vals {
        push_canonical_key(&mut out, v);
    }
    out
}

pub(crate) fn encode_one_into(out: &mut String, v: &Value) {
    encode_one_raw(out, v);
}

pub(crate) fn encode_key_refs_into(vals: &[&Value], out: &mut String) {
    encode_key_refs_into_in(vals, out, false);
}

/// v7.38.14 — key encode with a per-POSITION fold decision.
///
/// `encode_key_refs_into_in` takes one bool for the whole key, which
/// cannot express the case a join actually presents: one key column
/// declared `COLLATE utf8mb4_bin` beside another that folds. `folds` is
/// resolved once per join from the key columns' collations; a short or
/// missing entry means "do not fold", which is what every existing
/// caller wants.
pub(crate) fn encode_key_refs_folded(vals: &[&Value], out: &mut String, folds: &[bool]) {
    out.clear();
    for (i, v) in vals.iter().enumerate() {
        encode_one_in(out, v, folds.get(i).copied().unwrap_or(false));
    }
}

/// v7.39 (round 364, M4 P2) — key encode with the session dialect.
pub(crate) fn encode_key_refs_into_in(vals: &[&Value], out: &mut String, mysql: bool) {
    out.clear();
    for v in vals {
        encode_one_in(out, v, mysql);
    }
}

pub(crate) fn encode_key(vals: &[Value<'static>]) -> String {
    let mut out = String::new();
    for v in vals {
        encode_one(&mut out, v);
    }
    out
}

#[allow(clippy::cast_precision_loss)]
/// v7.37.17 (17.6 siblings) — intersect two ranges (same kind).
/// The greater lower bound wins (tie keeps inclusivity only when
/// both are inclusive); the smaller upper bound mirrors it; an
/// unbounded side loses to a bounded one. lower > upper — or a
/// touch that isn't inclusive on both ends — collapses to empty,
/// and any empty input pins the fold at empty.
fn range_intersect(a: &Value<'static>, b: &Value<'static>) -> Value<'static> {
    let (
        Value::Range {
            kind,
            lower: la,
            upper: ua,
            lower_inc: lia,
            upper_inc: uia,
            empty: ea,
        },
        Value::Range {
            lower: lb,
            upper: ub,
            lower_inc: lib_,
            upper_inc: uib,
            empty: eb,
            ..
        },
    ) = (a, b)
    else {
        return Value::Null;
    };
    let kind = *kind;
    let empty_range = Value::Range {
        kind,
        lower: None,
        upper: None,
        lower_inc: false,
        upper_inc: false,
        empty: true,
    };
    if *ea || *eb {
        return empty_range;
    }
    // Greater lower bound (None = -infinity loses to any bound).
    let (lower, lower_inc) = match (la, lb) {
        (None, None) => (None, false),
        (Some(x), None) => (Some(x.clone()), *lia),
        (None, Some(y)) => (Some(y.clone()), *lib_),
        (Some(x), Some(y)) => match value_cmp(x, y) {
            core::cmp::Ordering::Greater => (Some(x.clone()), *lia),
            core::cmp::Ordering::Less => (Some(y.clone()), *lib_),
            core::cmp::Ordering::Equal => (Some(x.clone()), *lia && *lib_),
        },
    };
    // Smaller upper bound (None = +infinity loses to any bound).
    let (upper, upper_inc) = match (ua, ub) {
        (None, None) => (None, false),
        (Some(x), None) => (Some(x.clone()), *uia),
        (None, Some(y)) => (Some(y.clone()), *uib),
        (Some(x), Some(y)) => match value_cmp(x, y) {
            core::cmp::Ordering::Less => (Some(x.clone()), *uia),
            core::cmp::Ordering::Greater => (Some(y.clone()), *uib),
            core::cmp::Ordering::Equal => (Some(x.clone()), *uia && *uib),
        },
    };
    if let (Some(lo), Some(up)) = (&lower, &upper) {
        match value_cmp(lo, up) {
            core::cmp::Ordering::Greater => return empty_range,
            core::cmp::Ordering::Equal if !(lower_inc && upper_inc) => {
                return empty_range;
            }
            _ => {}
        }
    }
    Value::Range {
        kind,
        lower,
        upper,
        lower_inc,
        upper_inc,
        empty: false,
    }
}

/// v7.38 (read01, T6.P3) — fold a NUMERIC input's kind into a running sum's kind:
/// NaN wins; ±Inf + finite → that Inf; +Inf + -Inf → NaN; else unchanged.
fn fold_sum_kind(
    acc: spg_storage::NumericKind,
    incoming: spg_storage::NumericKind,
) -> spg_storage::NumericKind {
    use spg_storage::NumericKind as NK;
    match (acc, incoming) {
        (NK::NaN, _) | (_, NK::NaN) => NK::NaN,
        (NK::Finite, k) | (k, NK::Finite) => k,
        (a, b) if a == b => a,
        _ => NK::NaN,
    }
}

/// v7.39 (enum order knife) — min/max extreme comparison: member order when
/// the spec's argument is enum-typed, the generic value order otherwise.
fn extreme_cmp(
    enum_labels: Option<&[String]>,
    a: &Value,
    b: &Value,
    mysql: bool,
) -> core::cmp::Ordering {
    extreme_cmp_in(enum_labels, None, a, b, mysql)
}

/// v7.39 (round 690) — `extreme_cmp` with the argument column's collation.
///
/// `min`/`max` over a column declared `COLLATE "en_US.utf8"` answered
/// `Banana` and `Ápple` where PG18 gives `apple` and `Zebra`. The collation
/// rides beside `enum_labels`, which is already exactly this: per-aggregate
/// metadata about the argument, resolved once where the spec is built.
///
/// No derivation needed here — `min(loc)`'s argument is the column itself.
/// An expression argument gets None and keeps byte order, which is the same
/// limit `ORDER BY upper(loc)` has.
fn extreme_cmp_in(
    enum_labels: Option<&[String]>,
    collation: Option<&str>,
    a: &Value,
    b: &Value,
    mysql: bool,
) -> core::cmp::Ordering {
    if let Some(labels) = enum_labels
        && let Some(ord) = crate::eval::enum_ord_cmp(labels, a, b)
    {
        return ord;
    }
    if let (Value::Text(x), Value::Text(y), Some(c)) = (a, b, collation)
        && let Some(ord) = crate::collate::compare(c, x, y)
    {
        return ord;
    }
    // v7.39 (round 412) — MIN / MAX over text under the MySQL default
    // collation compares by the folded form (case- and accent-insensitive,
    // PAD SPACE), matching ORDER BY (round 411).
    if mysql {
        // v7.38.18 — each side on its own type; see `mysql_fold_value`.
        if let (Some(x), Some(y)) = (
            spg_storage::mysql_fold_value(a),
            spg_storage::mysql_fold_value(b),
        ) {
            return x.cmp(&y);
        }
    }
    value_cmp(a, b)
}

/// Compare two values for `min` / `max`.
///
/// v7.39 (round 674) — the 228 lines that used to live here were a SECOND
/// comparison matrix, written independently of `orderby::value_cmp`. A
/// census of which `Value` variants each named found them diverged rather
/// than duplicated, and two silent wrongs fell out of the gap: `ORDER BY
/// time_col` did not sort (round 672) and `min`/`max` over `CHAR(n)`
/// returned the first row (round 672). Round 673 found four more on the
/// orderby side, where a canonical-text fallback had `ORDER BY money`
/// putting $100 before $9.
///
/// What stays here is the ONLY thing the two legitimately disagreed about:
/// where NULL sorts. This one puts NULLs last so `min`/`max` skip them;
/// `orderby::value_cmp` puts them first and the ORDER BY layer above it
/// applies NULLS FIRST / NULLS LAST. Both were correct in context, which is
/// why merging the matrices wholesale would have flipped one of them —
/// verified before collapsing, not after, and the eight NULL shapes are
/// pinned.
fn value_cmp(a: &Value, b: &Value) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        // NULLs last, so a NULL never wins a min() or a max().
        (Value::Null, _) => Ordering::Greater,
        (_, Value::Null) => Ordering::Less,
        _ => crate::orderby::value_cmp(a, b),
    }
}

/// v7.37.9 Phase 0 diagnostic counters — see
/// `.claude/notes/v7.37.9-class-a-c-cascade-closure-plan.md`. These
/// are read-only telemetry, do not gate any code path. Used by
/// `xtests/dogfood_replay/src/bin/counter_dump.rs` to verify
/// whether the DISTA A-3 + array_agg-ordered fast paths actually
/// fire on the mailrs Class A SQL shape.
pub static DISTA_LITERAL_ARG2_CACHE_FIRE: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static AGGREGATE_ARRAY_AGG_ORDER_BY_FIRE: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// v7.37.9 Phase 1A-ext — per-row spec dispatch branches in
/// `accumulate_groups`'s hot loop. Verifies the Phase 1A
/// decomposition agent's S06 assumption ("14 specs × eval_expr per
/// row"). Sum should equal `n_specs × n_input_rows`. Branch
/// distribution tells which attack target ROI is highest:
/// FAST_POS many = baseline OK; COMPILED_MISS many = Step-VM is
/// hot path; EVAL_FALLBACK > 0 = uncompilable specs walking the
/// eval_expr tree per row × Cow row materialise.
pub static AGG_PER_ROW_FAST_POS: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static AGG_PER_ROW_COMPILED_HIT: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static AGG_PER_ROW_COMPILED_MISS: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static AGG_PER_ROW_EVAL_FALLBACK: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static AGG_PER_ROW_COUNT_STAR_SENTINEL: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
mod value_cmp_mixed_numeric_tests {
    //! v7.37.16 Slice A — direct coverage of the mixed NUMERIC↔int/float
    //! arms in the aggregate-local `value_cmp` (drives min / max / argmin
    //! / argmax / mode / ordered-set aggregates). These pairs previously
    //! hit `_ => Equal`, which made `min`/`max` over a mixed NUMERIC/int
    //! key keep whichever row arrived first. Semantics now mirror
    //! binop.rs: int→NUMERIC exact promotion, NUMERIC→f64 demotion vs a
    //! float.
    use super::value_cmp;
    use core::cmp::Ordering;
    use spg_storage::Value;

    fn num(scaled: i128, scale: u16) -> Value<'static> {
        Value::Numeric {
            scaled,
            scale,
            kind: spg_storage::NumericKind::Finite,
        }
    }

    #[test]
    fn numeric_vs_integer_and_float() {
        assert_eq!(value_cmp(&num(250, 2), &Value::Int(5)), Ordering::Less);
        assert_eq!(value_cmp(&Value::Int(5), &num(250, 2)), Ordering::Greater);
        // debug-string/Equal fallback bug: 1000 vs 9 must be Greater.
        assert_eq!(
            value_cmp(&num(1000, 0), &Value::SmallInt(9)),
            Ordering::Greater
        );
        assert_eq!(value_cmp(&num(20, 1), &Value::BigInt(2)), Ordering::Equal);
        assert_eq!(value_cmp(&Value::BigInt(2), &num(20, 1)), Ordering::Equal);
        // NUMERIC↔float demotion.
        assert_eq!(value_cmp(&num(35, 1), &Value::Float(3.5)), Ordering::Equal);
        assert_eq!(
            value_cmp(&num(35, 1), &Value::Float(3.0)),
            Ordering::Greater
        );
        assert_eq!(value_cmp(&Value::Float(1.0), &num(25, 1)), Ordering::Less);
    }

    /// v7.39 (round 231) — `is_aggregate_name` admits a name and
    /// `classify_agg_name` panics on anything it doesn't know, so the two
    /// lists drifting apart turns into a SQL-reachable abort. That is how
    /// `every(x) OVER (…)` crashed the query in round 230. Walk the whole
    /// admitted set and classify each one.
    #[test]
    fn every_aggregate_name_classifies() {
        const NAMES: &[&str] = &[
            "count",
            "count_star",
            "sum",
            "min",
            "max",
            "avg",
            "any_value",
            "range_agg",
            "range_intersect_agg",
            "string_agg",
            "group_concat",
            "xmlagg",
            "array_agg",
            "bool_and",
            "bool_or",
            "every",
            "stddev",
            "stddev_samp",
            "stddev_pop",
            "variance",
            "var_samp",
            "var_pop",
            "bit_and",
            "bit_or",
            "bit_xor",
            "json_agg",
            "jsonb_agg",
            "json_object_agg",
            "jsonb_object_agg",
        ];
        for n in NAMES {
            assert!(
                super::is_aggregate_name(n),
                "{n} should be an aggregate name"
            );
            // Panics if the classifier doesn't know it.
            let _ = super::classify_agg_name(super::canonical_agg_name(n));
        }
        // Anything `is_aggregate_name` admits must classify, so a name added
        // to one list and not the other fails here rather than at runtime.
        for n in NAMES {
            assert!(
                super::is_aggregate_name(&n.to_ascii_uppercase()),
                "{n} should be case-insensitive"
            );
        }
    }
}
