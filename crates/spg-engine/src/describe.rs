//! v6.3.3 — Describe statement pre-Execute.
//!
//! Given a `Statement` returned by `Engine::prepare()`, compute
//! `(parameter_oids, output_columns)` without executing the
//! statement.
//!
//! Implementation policy:
//! - `parameter_oids`: count distinct `$N` placeholders in the AST
//!   and return a Vec<u32> of zeros (oid=0 = "let the server infer
//!   at Bind time"). PG drivers happily accept this.
//! - `output_columns`: resolve the FROM namespace — table, view,
//!   CTE, derived table, and every joined relation — then describe
//!   each SELECT item against it. Anything that cannot be resolved
//!   collapses the whole list to empty, which the pgwire layer maps
//!   to a `NoData` reply.
//!
//! v7.39 (round 462) — "complex shapes degrade to NoData, which
//! drivers tolerate" was wrong, and the cost was silent.
//!
//! Execute never sends a RowDescription (Describe owns it), so a
//! shape Describe cannot resolve reaches an extended-protocol client
//! as data rows with NO column metadata at all. Measured against
//! PG18 over sqlx: a view, a JOIN, a derived table, a UNION, a CTE
//! and every system catalog view all declared zero columns, so
//! `row.get(0)` was out of bounds on rows that plainly carried
//! values. Only a bare single-table SELECT worked. PG18 declares all
//! of them.
//!
//! Describe and execution must therefore agree by construction —
//! [`crate::tests`] pins the two against each other over a shape
//! corpus so a future shape cannot drift the way views did.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use spg_sql::ast::{Expr, Literal, SelectItem, SelectStatement, Statement, UnOp};
use spg_storage::{Catalog, ColumnSchema, DataType, Value};

/// One-shot describe of a prepared `Statement`.
///
/// Returns `(parameter_oids, output_columns)`. Empty `output_columns`
/// means "no row description available" → pgwire sends NoData.
pub fn describe_prepared(stmt: &Statement, catalog: &Catalog) -> (Vec<u32>, Vec<ColumnSchema>) {
    let params = collect_parameter_oids(stmt, catalog);
    let columns = describe_output_columns(stmt, catalog);
    (params, columns)
}

/// A relation chain deep enough to hit this is either pathological or
/// a cycle the catalog should not contain; stop rather than recurse.
const MAX_DESCRIBE_DEPTH: usize = 16;

fn describe_output_columns(stmt: &Statement, catalog: &Catalog) -> Vec<ColumnSchema> {
    // r1049 — DML with RETURNING produces a result set, and a driver
    // sizes its rows by THIS answer. Describing it as NoData made
    // `INSERT … RETURNING id` through sqlx come back as a zero-column
    // row (ColumnIndexOutOfBounds), a defect the sqlx suite had pinned
    // since v7.9 — in a test that had never run.
    let (table, items) = match stmt {
        Statement::Select(s) => return describe_select_columns(s, catalog, &[], 0),
        Statement::Insert(i) => (&i.table, i.returning.as_ref()),
        Statement::Update(u) => (&u.table, u.returning.as_ref()),
        Statement::Delete(d) => (&d.table, d.returning.as_ref()),
        _ => return Vec::new(),
    };
    let Some(items) = items else {
        return Vec::new();
    };
    dml_returning_columns(table, items, catalog)
}

/// The output columns of a DML statement's RETURNING list, resolved
/// against its target table. Shared by the statement standing alone
/// (r1049) and by a data-modifying CTE inside a SELECT (r1050) — one
/// answer, wherever the statement sits.
fn dml_returning_columns(
    table: &str,
    items: &[SelectItem],
    catalog: &Catalog,
) -> Vec<ColumnSchema> {
    let Some(t) = catalog.get(table) else {
        return Vec::new();
    };
    describe_select_items(items, &t.schema().columns, catalog)
}

/// Output columns of one SELECT, resolved against `catalog` plus any
/// CTEs already in scope from an enclosing query.
pub(crate) fn describe_select_columns(
    s: &SelectStatement,
    catalog: &Catalog,
    outer_ctes: &[&spg_sql::ast::Cte],
    depth: usize,
) -> Vec<ColumnSchema> {
    if depth > MAX_DESCRIBE_DEPTH {
        return Vec::new();
    }
    // A CTE shadows an outer binding of the same name, so this query's
    // own list goes first — `relation_columns` takes the first match.
    let mut ctes: Vec<&spg_sql::ast::Cte> = s.ctes.iter().collect();
    ctes.extend(outer_ctes.iter());

    // No FROM (`SELECT 1::INT AS one`) → describe items against an
    // empty namespace; literal / cast / function items still resolve.
    let ns = match &s.from {
        None => Vec::new(),
        Some(from) => {
            let Some(mut ns) = relation_columns(&from.primary, catalog, &ctes, depth) else {
                return Vec::new();
            };
            for j in &from.joins {
                let Some(cols) = relation_columns(&j.table, catalog, &ctes, depth) else {
                    return Vec::new();
                };
                ns.extend(cols);
            }
            ns
        }
    };
    // `t.*` over a multi-relation FROM cannot be answered from the flat
    // namespace — it carries no record of which columns came from which
    // relation, so expanding it would describe every column instead of
    // t's. NoData is the honest answer until the namespace is keyed.
    if s.from.as_ref().is_some_and(|f| !f.joins.is_empty())
        && s.items
            .iter()
            .any(|i| matches!(i, SelectItem::QualifiedWildcard(_)))
    {
        return Vec::new();
    }
    let out = describe_select_items(&s.items, &ns, catalog);
    if out.is_empty() {
        return out;
    }
    // A set operation takes its column NAMES from the first arm, so
    // `out` is already right — but only if the arms actually line up.
    // A width disagreement means the query will fail at execution;
    // describing it as if it succeeded would be worse than NoData.
    for (_, arm) in &s.unions {
        if describe_select_columns(arm, catalog, &ctes, depth + 1).len() != out.len() {
            return Vec::new();
        }
    }
    out
}

/// Columns one FROM entry contributes to the namespace, or `None` when
/// the relation cannot be resolved (the caller then reports NoData
/// rather than describing a partial namespace).
fn relation_columns(
    t: &spg_sql::ast::TableRef,
    catalog: &Catalog,
    ctes: &[&spg_sql::ast::Cte],
    depth: usize,
) -> Option<Vec<ColumnSchema>> {
    // A derived table (`FROM (SELECT …) x`, LATERAL or not) rides the
    // lateral_subquery channel.
    if let Some(sub) = &t.lateral_subquery {
        let cols = describe_select_columns(sub, catalog, &[], depth + 1);
        return (!cols.is_empty()).then_some(cols);
    }
    if let Some(table) = catalog.get(&t.name) {
        return Some(table.schema().columns.clone());
    }
    if let Some(cte) = ctes.iter().find(|c| c.name == t.name) {
        // r1050 — a data-modifying CTE is described by its RETURNING
        // list, the same answer Describe gives the statement standing
        // alone. sentori's report 3: the outer SELECT of
        // `WITH up AS (INSERT … RETURNING id) SELECT up.id, …` came
        // back undescribed, sqlx sized the row at zero columns, and
        // their suite stopped on step 16 — the exact family of the
        // r1049 defect, one level of nesting deeper.
        let mut cols = match &cte.body {
            spg_sql::ast::CteBody::Select(body) => {
                describe_select_columns(body, catalog, &[], depth + 1)
            }
            spg_sql::ast::CteBody::Insert(i) => {
                dml_returning_columns(&i.table, i.returning.as_ref()?, catalog)
            }
            spg_sql::ast::CteBody::Update(u) => {
                dml_returning_columns(&u.table, u.returning.as_ref()?, catalog)
            }
            spg_sql::ast::CteBody::Delete(d) => {
                dml_returning_columns(&d.table, d.returning.as_ref()?, catalog)
            }
            // MERGE RETURNING may project merge_action() and both
            // aliases; that shape has no describe path yet, and a
            // wrong answer here is worse than NoData.
            spg_sql::ast::CteBody::Merge(_) => return None,
        };
        if cols.is_empty() {
            return None;
        }
        // `WITH name(a, b, c)` renames positionally.
        if !cte.column_overrides.is_empty() && cte.column_overrides.len() == cols.len() {
            for (slot, name) in cols.iter_mut().zip(cte.column_overrides.iter()) {
                slot.name = name.clone();
            }
        }
        return Some(cols);
    }
    if catalog.has_view(&t.name) {
        let cols = describe_view_columns_depth(catalog, &t.name, depth + 1);
        return (!cols.is_empty()).then_some(cols);
    }
    // v7.38.3 — a set-returning function in FROM. The parser lowers
    // `jsonb_object_keys(x) AS key`, `unnest(a)`, `string_to_table(...)`
    // and the rest onto `unnest_expr` and already resolved the column
    // NAMES into `unnest_column_aliases` (PG's rules for which live
    // there). Only the types were missing here, so any statement with
    // one of these in FROM described NOTHING — sentori's project
    // context-keys page, one step past where their suite stands.
    //
    // The element type is the array expression's type with one level
    // peeled; anything this build cannot name that way is TEXT, which
    // is what the SRFs in question actually return.
    if let Some(arr) = &t.unnest_expr {
        let elem = describe_expr_type(arr, &[]).map_or(DataType::Text, |ty| match ty {
            DataType::IntArray => DataType::Int,
            DataType::SmallIntArray => DataType::SmallInt,
            DataType::BigIntArray => DataType::BigInt,
            DataType::BoolArray => DataType::Bool,
            DataType::UuidArray => DataType::Uuid,
            DataType::FloatArray => DataType::Float,
            DataType::NumericArray => DataType::Numeric {
                precision: 0,
                scale: 0,
            },
            DataType::DateArray => DataType::Date,
            DataType::TimestampArray => DataType::Timestamp,
            DataType::TimestamptzArray => DataType::Timestamptz,
            DataType::JsonArray => DataType::Json,
            DataType::JsonbArray => DataType::Jsonb,
            DataType::BytesArray => DataType::Bytes,
            DataType::IntervalArray => DataType::Interval,
            _ => DataType::Text,
        });
        // The FROM-SRF rewrite resolves the column name into the alias
        // list; a bare `unnest(a) AS v` leaves it empty and carries the
        // name on the item itself, which is also what the executor
        // projects.
        let first = t
            .unnest_column_aliases
            .first()
            .cloned()
            .unwrap_or_else(|| t.name.clone());
        let mut cols = alloc::vec![ColumnSchema::new(first, elem, true)];
        // WITH ORDINALITY appends a BIGINT counter, named by the second
        // alias when the caller gave one.
        if t.with_ordinality {
            let name = t
                .unnest_column_aliases
                .get(1)
                .cloned()
                .unwrap_or_else(|| "ordinality".to_string());
            cols.push(ColumnSchema::new(name, DataType::BigInt, false));
        }
        return Some(cols);
    }
    // VALUES / anything else the executor synthesises has no catalog
    // shape to read here.
    None
}

fn describe_select_items(
    items: &[SelectItem],
    schema_cols: &[ColumnSchema],
    cat: &Catalog,
) -> Vec<ColumnSchema> {
    let mut out: Vec<ColumnSchema> = Vec::with_capacity(items.len());
    for item in items {
        match item {
            // A qualified wildcard over a single relation describes the
            // same as `*`; the multi-relation case is refused above.
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                for c in schema_cols {
                    out.push(c.clone());
                }
            }
            SelectItem::Expr { expr, alias } => {
                // r1053 — a subquery in the SELECT list describes.
                // Any one of these made the WHOLE describe answer
                // empty, and sqlx sizes rows by Describe:
                // `SELECT (SELECT 1) AS one` came back as a zero-column
                // row (sentori report 4, their third Describe wall —
                // steps 30/86). EXISTS is a non-null boolean; a scalar
                // subquery carries its inner column's name and type,
                // nullable because an empty inner answer is NULL.
                let shape = match expr {
                    Expr::Exists { .. } => Some(ExprShape {
                        name: "exists".to_string(),
                        ty: DataType::Bool,
                        nullable: false,
                    }),
                    Expr::InSubquery { .. }
                    | Expr::RowInSubquery { .. }
                    | Expr::RowCmpSubquery { .. } => Some(ExprShape {
                        name: "?column?".to_string(),
                        ty: DataType::Bool,
                        nullable: true,
                    }),
                    Expr::ScalarSubquery(inner) => {
                        let cols = describe_select_columns(inner, cat, &[], 0);
                        match cols.as_slice() {
                            [c] => Some(ExprShape {
                                name: c.name.clone(),
                                ty: c.ty,
                                nullable: true,
                            }),
                            _ => None,
                        }
                    }
                    _ => describe_expr(expr, schema_cols),
                };
                let Some(desc) = shape else {
                    return Vec::new();
                };
                let name = alias.clone().unwrap_or(desc.name);
                out.push(ColumnSchema {
                    collation_name: None,
                    user_composite_type: None,
                    acl: alloc::vec::Vec::new(),
                    name,
                    ty: desc.ty,
                    nullable: desc.nullable,
                    auto_increment: false,
                    default: None,
                    runtime_default: None,
                    user_enum_type: None,
                    user_domain_type: None,
                    on_update_runtime: None,
                    collation: spg_storage::Collation::Binary,
                    is_unsigned: false,
                    inline_enum_variants: None,
                    inline_set_variants: None,
                    generated_stored_expr: None,
                    identity_always: false,
                    default_text: None,
                    auto_restart: None,
                    scalar_row_source: false,
                    mysql_int_width: None,
                    mysql_fsp: None,
                });
            }
        }
    }
    out
}

/// v7.39 (round 268) — a view's output columns, resolved from its
/// stored body. `information_schema.columns` had no rows at all for a
/// view before this, so a reflection tool saw every view as a relation
/// with no columns.
///
/// The namespace a view body resolves against is wider than the
/// prepared-statement describe path builds: the primary may itself be a
/// view (recurse), and joined tables contribute their columns too. An
/// item that cannot be resolved collapses the whole list to empty,
/// which the caller reports as "no columns known" rather than guessing.
pub(crate) fn describe_view_columns(catalog: &Catalog, view_name: &str) -> Vec<ColumnSchema> {
    describe_view_columns_depth(catalog, view_name, 0)
}

fn describe_view_columns_depth(
    catalog: &Catalog,
    view_name: &str,
    depth: usize,
) -> Vec<ColumnSchema> {
    if depth > MAX_DESCRIBE_DEPTH {
        return Vec::new();
    }
    let Some(view) = catalog.view(view_name) else {
        return Vec::new();
    };
    let Ok(Statement::Select(select)) = spg_sql::parser::parse_statement(&view.body) else {
        return Vec::new();
    };
    // v7.39 (round 462) — the body resolves through the same namespace
    // walk a top-level SELECT uses, so a view over a join, a derived
    // table or a CTE describes exactly as that query would.
    let mut out = describe_select_columns(&select, catalog, &[], depth);
    // A rename list overrides the body's own names, positionally.
    if !view.columns.is_empty() && view.columns.len() == out.len() {
        for (slot, name) in out.iter_mut().zip(view.columns.iter()) {
            slot.name = name.clone();
        }
    }
    // Every column of a view is nullable in PG, even where the base
    // column is NOT NULL: the view's rows are a query result, and PG
    // does not carry the base constraint through.
    for c in &mut out {
        c.nullable = true;
    }
    out
}

pub(crate) struct ExprShape {
    pub(crate) name: String,
    pub(crate) ty: DataType,
    pub(crate) nullable: bool,
}

/// v7.38 (read01) — PG numeric-category width rank for common-type
/// resolution: smallint < int < bigint < numeric < double precision.
/// `real` (float4) is deliberately omitted: PG's preferred-type rules make
/// some real mixes resolve to `real` and others to `double precision`, so a
/// real-involving mix is left uncoerced rather than risk the wrong widening.
pub(crate) fn numeric_rank(t: DataType) -> Option<u8> {
    match t {
        DataType::SmallInt => Some(1),
        DataType::Int => Some(2),
        DataType::BigInt => Some(3),
        DataType::Numeric { .. } => Some(4),
        // v7.39 (round 649) — rank 5 was left empty for `real` and never
        // filled, so any sibling set containing one failed the
        // "all numeric" test and fell through untouched. Measured:
        // `coalesce(1::real, 1.5::float8)` answered `real` where PG says
        // `double precision`, and `coalesce(1::int, 1::real)` answered
        // `integer` where PG says `real`.
        DataType::Real => Some(5),
        DataType::Float => Some(6),
        _ => None,
    }
}

/// v7.38 (read01) — the PG common type for a set of sibling branch/argument
/// types (`CASE`, `COALESCE`, `GREATEST`/`LEAST`, `NULLIF`). A safe subset of
/// PG's type resolution; returns `None` for anything ambiguous so the caller
/// leaves the value untouched (never turns a working expression into one that
/// coerces wrongly or errors):
///   * all numeric-category → the widest (int ∪ numeric → numeric,
///     … ∪ float8 → float8);
///   * DATE/TIMESTAMP/TIMESTAMPTZ with at least one timestamp[tz] →
///     timestamptz if any is tz, else timestamp (all are the same UTC
///     instant, so widening is lossless);
///   * exactly one concrete non-TEXT type mixed with TEXT → that type.
pub(crate) fn common_type(types: &[DataType]) -> Option<DataType> {
    let mut distinct: Vec<&DataType> = Vec::new();
    for t in types {
        if !distinct.iter().any(|d| *d == t) {
            distinct.push(t);
        }
    }
    if distinct.len() < 2 {
        return None;
    }
    if distinct.iter().all(|t| numeric_rank(**t).is_some()) {
        return distinct
            .iter()
            .max_by_key(|t| numeric_rank(***t).unwrap_or(0))
            .map(|t| *(*t));
    }
    let non_text: Vec<&DataType> = distinct
        .iter()
        .copied()
        .filter(|t| !matches!(t, DataType::Text))
        .collect();
    if non_text.iter().all(|t| {
        matches!(
            t,
            DataType::Date | DataType::Timestamp | DataType::Timestamptz
        )
    }) && non_text
        .iter()
        .any(|t| matches!(t, DataType::Timestamp | DataType::Timestamptz))
    {
        if non_text.iter().any(|t| matches!(t, DataType::Timestamptz)) {
            return Some(DataType::Timestamptz);
        }
        return Some(DataType::Timestamp);
    }
    if non_text.len() == 1 {
        return Some(*non_text[0]);
    }
    None
}

/// v7.39 (round 609) — a literal's type and nullability, split out so the
/// type-only entry point below reads it without building a shape name.
fn literal_type(lit: &spg_sql::ast::Literal) -> Option<(DataType, bool)> {
    use spg_sql::ast::Literal as L;
    let (ty, nullable) = match lit {
        L::Null => (DataType::Text, true),
        // Array literals only enter the AST via the
        // prepared-bind path; surface as TEXT (no array
        // DataType in the describe surface yet).
        L::TextArray(_) | L::IntArray(_) | L::BigIntArray(_) => (DataType::Text, false),
        // PG-canonical literal-int typing: `pg_typeof(1) =
        // integer`, `pg_typeof(2147483648) = bigint`. The
        // engine's runtime Value::Int(i32) flows naturally
        // into INT columns; widening to BIGINT happens in
        // coerce_value only when the column type asks for
        // it. Bisected to P0-4: pre-fix every literal was
        // BigInt, which let `WITH RECURSIVE t(n) AS (SELECT
        // 1 …)` infer the working table column as BIGINT
        // while the second-iteration INSERT path produced a
        // Value::Int(1) — type mismatch.
        L::Integer(n) => {
            if i32::try_from(*n).is_ok() {
                (DataType::Int, false)
            } else {
                (DataType::BigInt, false)
            }
        }
        L::Float(_) => (DataType::Float, false),
        L::Numeric { .. } => (
            DataType::Numeric {
                precision: 0,
                scale: 0,
            },
            false,
        ),
        L::NumericBig(_) => (
            DataType::Numeric {
                precision: 0,
                scale: 0,
            },
            false,
        ),
        L::String(_) => (DataType::Text, false),
        L::Bool(_) => (DataType::Bool, false),
        L::Vector(_) | L::Interval { .. } => return None,
    };
    Some((ty, nullable))
}

/// v7.39 (round 609) — the TYPE alone, without building the shape's name.
///
/// `unify_branch_types_static` runs for every row of a COALESCE / GREATEST /
/// LEAST and only ever reads `.ty`, but `describe_expr` clones a column's
/// name (or writes "?column?") to hand it back: two allocations a row for
/// `coalesce(id, 0)`, whose answer needs none.
pub(crate) fn describe_expr_type(e: &Expr, schema_cols: &[ColumnSchema]) -> Option<DataType> {
    match e {
        Expr::Column(c) => {
            // The same lookup `describe_expr` does, minus the name clone.
            if let Some(col) = schema_cols.iter().find(|s| s.name == c.name) {
                return Some(col.ty);
            }
            describe_expr(e, schema_cols).map(|s| s.ty)
        }
        Expr::Literal(lit) => literal_type(lit).map(|(ty, _)| ty),
        _ => describe_expr(e, schema_cols).map(|s| s.ty),
    }
}

pub(crate) fn describe_expr(e: &Expr, schema_cols: &[ColumnSchema]) -> Option<ExprShape> {
    match e {
        Expr::Column(c) => {
            // Mirror resolve_projection_column's lookup: bare name first,
            // then qualified-prefix match.
            let bare = schema_cols.iter().find(|s| s.name == c.name);
            if let Some(col) = bare {
                return Some(ExprShape {
                    name: c.name.clone(),
                    ty: col.ty,
                    nullable: col.nullable,
                });
            }
            let suffix = alloc::format!(".{}", c.name);
            let mut matches = schema_cols.iter().filter(|s| s.name.ends_with(&suffix));
            let first = matches.next()?;
            if matches.next().is_some() {
                // ambiguous — bail (describe should not assume an
                // arbitrary tiebreak)
                return None;
            }
            Some(ExprShape {
                name: c.name.clone(),
                ty: first.ty,
                nullable: first.nullable,
            })
        }
        Expr::Literal(lit) => {
            let (ty, nullable) = literal_type(lit)?;
            Some(ExprShape {
                name: "?column?".to_string(),
                ty,
                nullable,
            })
        }
        Expr::Cast { target, .. } => {
            use spg_sql::ast::CastTarget;
            let ty = match target {
                CastTarget::Int => DataType::Int,
                CastTarget::BigInt => DataType::BigInt,
                CastTarget::Float => DataType::Float,
                CastTarget::Text => DataType::Text,
                CastTarget::Bool => DataType::Bool,
                CastTarget::Vector => return None,
                CastTarget::Date => DataType::Date,
                CastTarget::Timestamp => DataType::Timestamp,
                CastTarget::Timestamptz => DataType::Timestamptz,
                CastTarget::Interval => DataType::Interval,
                CastTarget::Json => DataType::Json,
                CastTarget::Jsonb => DataType::Jsonb,
                // regtype / regclass yield text-shape catalog OIDs
                // on PG; on SPG the engine surfaces Unsupported,
                // but for describe we still claim Text so prepare
                // doesn't fail.
                CastTarget::RegType | CastTarget::RegClass => DataType::Text,
                CastTarget::TextArray => DataType::TextArray,
                CastTarget::IntArray => DataType::IntArray,
                CastTarget::BigIntArray => DataType::BigIntArray,
                // v7.12.0 — `::tsvector` / `::tsquery`.
                CastTarget::TsVector => DataType::TsVector,
                CastTarget::TsQuery => DataType::TsQuery,
                CastTarget::Uuid => DataType::Uuid,
                CastTarget::Bytea => DataType::Bytes,
                // v7.37.5 — generic typed-cast escape. Resolve the
                // ident to a `DataType` for prepare-time schema
                // information; truly-unknown idents bail (describe
                // returns None so prepare reports an Unsupported).
                CastTarget::Named(name) => crate::conversions::type_name_to_data_type(name)?,
            };
            Some(ExprShape {
                name: "?column?".to_string(),
                ty,
                nullable: true,
            })
        }
        // v7.38.3 (sentori step 41) — the predicate shapes, every one of
        // which is BOOLEAN in PG (verified against PG18's own view
        // columns, 2026-08-19). They had no arm at all, so a select item
        // whose TOP-LEVEL expression was one of them fell to the `None`
        // below, and Describe answered the whole statement with NO
        // COLUMNS — a client asking what `SELECT id, k IS NOT NULL AS
        // addressable FROM …` returns was told nothing at all. Nested
        // inside another operator they already described, because the
        // arm that consumed them (Binary, Case) never asked what they
        // were; that is why `(a IS NOT NULL AND b IS NOT NULL)` worked
        // and `a IS NOT NULL` did not.
        //
        // sentori hit three of these; the rest were the same hole and
        // are closed with them rather than left for the next report.
        Expr::IsNull { .. }
        | Expr::BoolTest { .. }
        | Expr::Like { .. }
        | Expr::InList { .. }
        | Expr::Unary { op: UnOp::Not, .. } => Some(ExprShape {
            name: "?column?".to_string(),
            ty: DataType::Bool,
            nullable: true,
        }),
        // Unary `~` and `+` keep the operand's type, as unary minus does.
        Expr::Unary {
            op: UnOp::BitNot | UnOp::Plus,
            expr,
        } => {
            let inner = describe_expr(expr, schema_cols)?;
            Some(ExprShape {
                name: "?column?".to_string(),
                ty: inner.ty,
                nullable: inner.nullable,
            })
        }
        // Unary minus preserves the operand's type.
        Expr::Unary {
            op: UnOp::Neg,
            expr,
        } => {
            let inner = describe_expr(expr, schema_cols)?;
            Some(ExprShape {
                name: "?column?".to_string(),
                ty: inner.ty,
                nullable: inner.nullable,
            })
        }
        // Function call — dispatch on name to recover the column
        // type that the wire layer (and sqlx::Column type_info)
        // advertises. Without this entry build_projection falls
        // back to `Text` for every non-trivial expression, which
        // breaks `sqlx::query_as::<_, (chrono::NaiveDateTime,)>(
        // "SELECT now()")` and every similar typed-decode pattern.
        // v7.38.3 — PG names an unaliased function call after the
        // FUNCTION, not `?column?`: `SELECT count(*)` describes a column
        // called `count`, `max(a)` one called `max` (measured on PG18's
        // own view columns). SPG's execute path already did this, so
        // Describe and the RowDescription that arrived on Execute
        // disagreed with each other — a client that reads names from
        // Describe looked for `count` and was told `?column?`. Only the
        // NAME is taken here; the type still comes from the map below.
        Expr::FunctionCall { name, args } => {
            function_return_shape(name, args, schema_cols).map(|mut sh| {
                if sh.name == "?column?" {
                    // `COUNT(*)` parses to the internal name `count_star`;
                    // PG calls the column `count`, which is what the user
                    // wrote. Never let an internal spelling reach the wire.
                    sh.name = if name.eq_ignore_ascii_case("count_star") {
                        "count".to_string()
                    } else {
                        name.to_ascii_lowercase()
                    };
                }
                sh
            })
        }
        // v7.26 (round-20 C) — aggregate modifiers delegate to the
        // inner call (DISTINCT / internal ORDER BY don't change the
        // output type).
        // v7.38.3 — the ordered-set aggregates. Delegating to the inner
        // call asked `function_return_shape` about `percentile_cont`,
        // which it did not know, and a None here empties the WHOLE
        // statement's Describe. PG's rule, measured: percentile_cont is
        // double precision (double precision[] when its direct argument
        // is an array of fractions), while percentile_disc and mode take
        // the type of the column being ordered by — which is why the
        // sort spec has to be read rather than dropped.
        Expr::AggregateOrdered { call, order_by, .. }
            if matches!(call.as_ref(), Expr::FunctionCall { name, .. }
            if matches!(name.to_ascii_lowercase().as_str(),
                        "percentile_cont" | "percentile_disc" | "mode")) =>
        {
            let Expr::FunctionCall { name, args } = call.as_ref() else {
                return None;
            };
            let lower = name.to_ascii_lowercase();
            let ty = if lower == "percentile_cont" {
                let fractions_are_array = args.first().is_some_and(|a| matches!(a, Expr::Array(_)));
                if fractions_are_array {
                    DataType::FloatArray
                } else {
                    DataType::Float
                }
            } else {
                // percentile_disc / mode return a value FROM the ordered
                // column, so they carry its type.
                order_by
                    .first()
                    .and_then(|o| describe_expr_type(&o.expr, schema_cols))?
            };
            Some(ExprShape {
                name: lower,
                ty,
                nullable: true,
            })
        }
        Expr::AggregateOrdered { call, .. } => describe_expr(call, schema_cols),
        // v7.39 (round 268) — a window call. The pure window functions
        // have fixed result types (measured on PG 18.4); everything else
        // over OVER() is an aggregate and keeps the aggregate's type, so
        // it delegates. Without this arm a view with any window column
        // resolved to nothing at all, and reported no columns.
        Expr::WindowFunction { name, args, .. } => {
            let lower = name.to_ascii_lowercase();
            let fixed = match lower.as_str() {
                "row_number" | "rank" | "dense_rank" => Some(DataType::BigInt),
                "ntile" => Some(DataType::Int),
                "percent_rank" | "cume_dist" => Some(DataType::Float),
                _ => None,
            };
            if let Some(ty) = fixed {
                return Some(ExprShape {
                    name: lower,
                    ty,
                    nullable: true,
                });
            }
            // lag / lead / first_value / last_value / nth_value report
            // their first argument's type.
            if matches!(
                lower.as_str(),
                "lag" | "lead" | "first_value" | "last_value" | "nth_value"
            ) {
                let inner = describe_expr(args.first()?, schema_cols)?;
                return Some(ExprShape {
                    name: lower,
                    ty: inner.ty,
                    nullable: true,
                });
            }
            let inner = function_return_shape(name, args, schema_cols)?;
            Some(ExprShape {
                name: lower,
                ty: inner.ty,
                nullable: true,
            })
        }
        // CASE — unify on the first THEN branch's shape (PG unifies
        // across branches; first-branch is the pragmatic subset).
        Expr::Case {
            branches,
            else_branch,
            ..
        } => {
            let probe = branches
                .first()
                .map(|(_, t)| t)
                .or(else_branch.as_deref())?;
            let inner = describe_expr(probe, schema_cols)?;
            Some(ExprShape {
                name: "case".to_string(),
                ty: inner.ty,
                nullable: true,
            })
        }
        // Binary — comparisons/logic → BOOL; everything else takes
        // the left operand's shape (PG's numeric promotion is finer,
        // but lhs covers the aggregate/projection metadata cases).
        Expr::Binary { lhs, op, rhs: _ } => {
            use spg_sql::ast::BinOp as B;
            match op {
                B::Eq | B::NotEq | B::Lt | B::LtEq | B::Gt | B::GtEq | B::And | B::Or => {
                    Some(ExprShape {
                        name: "?column?".to_string(),
                        ty: DataType::Bool,
                        nullable: true,
                    })
                }
                // v7.38.3 — the JSON operators that do NOT return JSON.
                // The fallback below takes the LEFT operand's type, so
                // `payload->'context'->>'k'` described as JSONB when PG
                // says TEXT (measured). A typed decode into String then
                // met a jsonb OID and failed at the client, with nothing
                // wrong on the wire to point at.
                B::JsonGetText | B::JsonGetPathText => Some(ExprShape {
                    name: "?column?".to_string(),
                    ty: DataType::Text,
                    nullable: true,
                }),
                // The predicate forms are BOOLEAN, same measurement.
                B::JsonContains
                | B::JsonContainedBy
                | B::JsonPathExists
                | B::JsonKeyExists
                | B::JsonKeysAny
                | B::JsonKeysAll => Some(ExprShape {
                    name: "?column?".to_string(),
                    ty: DataType::Bool,
                    nullable: true,
                }),
                _ => {
                    let inner = describe_expr(lhs, schema_cols)?;
                    // v7.38 (read01 A-bitcat) — PG's `||` on bit strings is
                    // `bitcat`, whose result is always `bit varying`: the
                    // operands widen to varbit and the concatenated length
                    // isn't a fixed `bit(N)`. So `B'10' || B'11'` is
                    // `bit varying`, matching PG's pg_typeof — not the left
                    // operand's `bit`.
                    let ty = if matches!(op, B::Concat)
                        && matches!(inner.ty, DataType::Bit(_) | DataType::BitVarying(_))
                    {
                        // The concatenation's length is not a fixed
                        // typmod, so it widens to unbounded varbit.
                        DataType::BitVarying(0)
                    } else {
                        inner.ty
                    };
                    Some(ExprShape {
                        name: "?column?".to_string(),
                        ty,
                        nullable: true,
                    })
                }
            }
        }
        // (array_agg(…))[1] — element type of the array.
        Expr::ArraySubscript { target, .. } => {
            let inner = describe_expr(target, schema_cols)?;
            let elem = match inner.ty {
                DataType::IntArray => DataType::Int,
                DataType::BigIntArray => DataType::BigInt,
                DataType::TextArray => DataType::Text,
                other => other,
            };
            Some(ExprShape {
                name: "?column?".to_string(),
                ty: elem,
                nullable: true,
            })
        }
        // arr[lo:hi] — slice keeps the array type.
        Expr::ArraySlice { target, .. } => describe_expr(target, schema_cols),
        // v7.37.43-T4 — `$N` placeholders in a projection. Pre-T4 this
        // arm fell through to `_ => None`, which made
        // `describe_select_items` return an empty Vec, which made
        // pgwire send `NoData` in response to Describe. But the same
        // placeholder DID produce a column at Execute time (engine
        // substitutes the bound value, the column appears in the
        // result), so pgwire then sent `RowDescription + DataRow`
        // anyway. The wire stream went `NoData` → `RowDescription` →
        // `DataRow` — a sequence libpq / sqlx-postgres / pg-jdbc don't
        // accept (NoData is a terminal answer: no rows ever). sqlx
        // hit `unexpected message: RowDescription` mid-stream, which
        // dispatched into its boxed-future error recovery and
        // stack-overflowed the calling thread.
        //
        // The repro is `SELECT $1` (literally any sqlx prepared SELECT
        // with a parameter) — including the `pg_advisory_lock($1)`
        // that `sqlx::migrate!()` issues right after `current_database()`.
        // Pre-T4 every sqlx user crashed on the very first parameterised
        // prepared SELECT.
        //
        // Fix: when describing a `$N` placeholder, return a Text shape
        // (oid 25 — the SQL text format that pgwire returns on the
        // text wire path) so Describe yields a RowDescription with one
        // column. The actual data type comes from the bound value at
        // Execute time; sqlx tolerates this because the text wire
        // format makes the per-column type advisory rather than load-
        // bearing. The column name "?column?" mirrors PG's own
        // canonical projection-of-an-expression name.
        Expr::Placeholder(_) => Some(ExprShape {
            name: "?column?".to_string(),
            ty: DataType::Text,
            nullable: true,
        }),
        _ => None,
    }
}

/// Static return-type map for the SQL function library. Returns
/// None for functions whose return type genuinely depends on
/// runtime values in a way the planner can't statically resolve
/// (e.g. `coalesce(arg1, arg2)` where arg1 is NULL literal — the
/// caller's type-inference cascade handles those).
fn function_return_shape(
    name: &str,
    args: &[Expr],
    schema_cols: &[ColumnSchema],
) -> Option<ExprShape> {
    let lc = name.to_ascii_lowercase();
    let (ty, nullable) = match lc.as_str() {
        // Time-of-now → engine clock literals.
        "now"
        | "current_timestamp"
        | "localtimestamp"
        | "transaction_timestamp"
        | "statement_timestamp"
        | "clock_timestamp" => (DataType::Timestamptz, false),
        "current_date" => (DataType::Date, false),
        // v7.39 (tz epic) — AT TIME ZONE flips the flavour: a naive
        // timestamp AT ZONE is a timestamptz, a timestamptz AT ZONE a
        // naive timestamp (PG).
        "timezone" if args.len() == 2 => {
            let src_is_tstz = args
                .get(1)
                .and_then(|a| describe_expr(a, schema_cols))
                .is_some_and(|s| matches!(s.ty, DataType::Timestamptz));
            (
                if src_is_tstz {
                    DataType::Timestamp
                } else {
                    DataType::Timestamptz
                },
                true,
            )
        }
        // v7.39 (round 755, F31-B6) — true TIME family, PG18-measured
        // (`time with time zone` / `time without time zone`).
        "current_time" => (DataType::TimeTz, false),
        "localtime" => (DataType::Time, false),
        // Text-returning library — every fn that produces a string.
        "concat"
        | "concat_ws"
        | "format"
        | "lower"
        | "upper"
        | "trim"
        | "ltrim"
        | "rtrim"
        | "substring"
        | "substr"
        | "replace"
        | "split_part"
        | "repeat"
        | "lpad"
        | "rpad"
        | "left"
        | "right"
        | "translate"
        | "regexp_replace"
        | "to_char"
        | "encode"
        | "host"
        | "network"
        | "version"
        | "database"
        | "current_database"
        | "current_schema"
        | "current_user"
        | "session_user"
        | "user"
        | "pg_get_serial_sequence"
        | "pg_get_constraintdef"
        | "pg_get_indexdef"
        | "date_format"
        | "pg_typeof" => (DataType::Text, true),
        // Bytes-returning.
        "decode" | "hex" => (DataType::Bytes, true),
        // Integer-returning length / position helpers.
        "length" | "char_length" | "character_length" | "octet_length" | "bit_length"
        | "position" | "strpos" | "ascii" | "masklen" => (DataType::Int, true),
        // BigInt-returning.
        "count" | "count_star" | "nextval" | "currval" | "lastval" | "unix_timestamp" => {
            (DataType::BigInt, true)
        }
        // Float / double-precision returns.
        "random" | "ts_rank" | "ts_rank_cd" | "similarity" | "ln" | "log" | "log2" | "exp"
        | "sin" | "cos" | "tan" | "asin" | "acos" | "atan" | "atan2" | "degrees" | "radians"
        | "pi" => (DataType::Float, true),
        // Boolean predicate-returning.
        "starts_with" => (DataType::Bool, true),
        // Arrays.
        "regexp_matches"
        | "regexp_split_to_array"
        | "show_trgm"
        | "string_to_array"
        | "array_remove"
        | "array_append"
        | "array_cat" => (DataType::TextArray, true),
        // JSON.
        "to_json"
        | "to_jsonb"
        | "json_build_object"
        | "jsonb_build_object"
        | "json_build_array"
        | "jsonb_build_array"
        | "json_object"
        | "jsonb_object"
        | "jsonb_set"
        | "jsonb_insert"
        | "jsonb_path_query"
        | "jsonb_path_query_first"
        | "jsonb_path_query_array"
        | "json_path_query" => (DataType::Json, true),
        // FTS types.
        "to_tsvector" => (DataType::TsVector, true),
        "to_tsquery" | "plainto_tsquery" | "phraseto_tsquery" | "websearch_to_tsquery" => {
            (DataType::TsQuery, true)
        }
        // v7.17.0 — UUID generators. `gen_random_uuid()` is the
        // PG built-in; `uuid_generate_v4()` is the historical
        // uuid-ossp alias. Both return a NOT NULL UUID — non-
        // nullable since neither takes args and neither can fail.
        "gen_random_uuid" | "uuid_generate_v4" => (DataType::Uuid, false),
        // Interval.
        "age" => (DataType::Interval, true),
        // Timestamp-returning. `from_unixtime` switches to TEXT
        // when called with a format-string second arg — handled
        // below via arity check.
        "make_timestamp" => (DataType::Timestamp, true),
        // v7.39 (read01 round 77) — date_trunc / date_bin return the type of
        // the timestamp they were HANDED (PG has a timestamptz overload of
        // each), so a truncated timestamptz keeps its `+00` on the way out.
        // Pinning them to Timestamp silently dropped the offset.
        // v7.39 (read01 round 114) — a `date` argument resolves to PG's
        // *timestamptz* overload (timestamptz is date's preferred implicit
        // cast), so `date_trunc('q', date '…')` is timestamptz (`…+00`), not a
        // plain timestamp. Only a bare `timestamp` stays timestamp.
        "date_trunc" | "date_bin" => {
            let src = args.get(1)?;
            let ty = describe_expr(src, schema_cols).map_or(DataType::Timestamp, |s| match s.ty {
                DataType::Timestamptz | DataType::Date => DataType::Timestamptz,
                _ => DataType::Timestamp,
            });
            (ty, true)
        }
        // v7.39 (round 522) — PG's `date_add` / `date_subtract` are
        // declared over timestamptz and answer timestamptz; the parser
        // writes the coercion PG performs, so the first argument carries
        // the answer. MySQL's DATE_ADD gets no such cast and keeps its
        // own DATE / DATETIME result.
        "date_add" | "date_subtract" => {
            // Only the PG-dialect form is typed here — the one whose
            // first argument the parser lifted to timestamptz. MySQL's
            // DATE_ADD gets no such cast and keeps the typing it had.
            let src = args.first()?;
            if !matches!(
                describe_expr(src, schema_cols).map(|s| s.ty),
                Some(DataType::Timestamptz)
            ) {
                return None;
            }
            (DataType::Timestamptz, true)
        }
        "from_unixtime" => {
            if args.len() >= 2 {
                (DataType::Text, true)
            } else {
                (DataType::Timestamp, true)
            }
        }
        "make_date" | "to_date" => (DataType::Date, true),
        // v7.39 (read01 formatting.c) — PG's to_timestamp (both the epoch
        // and the format form) returns timestamptz.
        "to_timestamp" => (DataType::Timestamptz, true),
        // v7.39 (read01 timestamp.c) — make_timestamptz returns tstz.
        "make_timestamptz" => (DataType::Timestamptz, true),
        // v7.39 (read01 uuid.c) — uuid_extract_timestamp returns tstz.
        "uuid_extract_timestamp" => (DataType::Timestamptz, true),
        "date_part" | "extract" => (DataType::Float, true),
        // v7.26 (round-20 C) — remaining aggregate signatures
        // (count / ts_rank were already mapped above). PG types
        // these from the aggregate's declaration; SPG used to
        // default them to TEXT, breaking sqlx typed decodes.
        "bool_and" | "bool_or" | "every" => (DataType::Bool, true),
        "string_agg" => (DataType::Text, true),
        "array_agg" => {
            let elem = args
                .first()
                .and_then(|a| describe_expr(a, schema_cols))
                .map(|s| s.ty);
            // v7.38.3 — every element type this build has an array type
            // for, not just the two integer widths. `array_agg(uuid_col)`
            // described as TEXT[] where PG says UUID[] (measured), so a
            // typed decode into Vec<Uuid> met the wrong OID and failed at
            // the client. TEXT[] stays the answer only for element types
            // with no array of their own here.
            let ty = match elem {
                Some(DataType::Int) => DataType::IntArray,
                Some(DataType::SmallInt) => DataType::SmallIntArray,
                Some(DataType::BigInt) => DataType::BigIntArray,
                Some(DataType::Bool) => DataType::BoolArray,
                Some(DataType::Uuid) => DataType::UuidArray,
                Some(DataType::Numeric { .. }) => DataType::NumericArray,
                Some(DataType::Float | DataType::Real) => DataType::FloatArray,
                Some(DataType::Date) => DataType::DateArray,
                Some(DataType::Timestamp) => DataType::TimestampArray,
                Some(DataType::Timestamptz) => DataType::TimestamptzArray,
                Some(DataType::Json) => DataType::JsonArray,
                Some(DataType::Jsonb) => DataType::JsonbArray,
                Some(DataType::Bytes) => DataType::BytesArray,
                Some(DataType::Interval) => DataType::IntervalArray,
                _ => DataType::TextArray,
            };
            (ty, true)
        }
        // v7.39 (read01 round 77) — the conditional family resolves a COMMON
        // type across its arguments, and an untyped NULL literal contributes
        // nothing to it. Taking `args[0]` unconditionally meant
        // `coalesce(NULL, <timestamptz>)` described as "no shape at all", so
        // the timestamptz lost its `+00` — the type was decided by argument
        // POSITION rather than by the arguments.
        "coalesce" | "greatest" | "least" | "ifnull" | "isnull" | "nullif" => {
            let shapes: Vec<ExprShape> = args
                .iter()
                .filter(|a| !matches!(a, Expr::Literal(Literal::Null)))
                .filter_map(|a| describe_expr(a, schema_cols))
                .collect();
            let first = shapes.first()?;
            let types: Vec<DataType> = shapes.iter().map(|s| s.ty).collect();
            return Some(ExprShape {
                name: "?column?".to_string(),
                ty: common_type(&types).unwrap_or(first.ty),
                nullable: true,
            });
        }
        // v7.39 (round 268) — sum / avg PROMOTE; they were lumped in
        // with the pass-through math below and reported the argument's
        // own type. The runtime has always promoted correctly
        // (sum(int) really does return bigint), so this was a static
        // description that disagreed with the value the engine sends —
        // a driver that trusts the RowDescription decodes an int4 and
        // gets eight bytes. All types measured on PG 18.4.
        "sum" => {
            let inner = describe_expr(args.first()?, schema_cols)?;
            let ty = match inner.ty {
                DataType::SmallInt | DataType::Int => DataType::BigInt,
                DataType::BigInt => DataType::Numeric {
                    precision: 0,
                    scale: 0,
                },
                other => other,
            };
            return Some(ExprShape {
                name: "?column?".to_string(),
                ty,
                nullable: true,
            });
        }
        "avg" => {
            let inner = describe_expr(args.first()?, schema_cols)?;
            let ty = match inner.ty {
                DataType::SmallInt | DataType::Int | DataType::BigInt => DataType::Numeric {
                    precision: 0,
                    scale: 0,
                },
                // real averages as double precision, unlike sum, which
                // stays real.
                DataType::Real => DataType::Float,
                other => other,
            };
            return Some(ExprShape {
                name: "?column?".to_string(),
                ty,
                nullable: true,
            });
        }
        // Pass-through math: derive the type from the first arg.
        "max" | "min" | "abs" | "floor" | "ceil" | "ceiling" | "round" | "trunc" | "mod"
        | "power" | "pow" | "sqrt" | "sign" => {
            // Use the first arg's shape; fall back to Float for math
            // that can promote (e.g. mod(2, 3) → Float? No — keep
            // Int. The caller's coerce_value handles promotion at
            // INSERT time.)
            let first = args.first()?;
            let inner = describe_expr(first, schema_cols)?;
            return Some(ExprShape {
                name: "?column?".to_string(),
                ty: inner.ty,
                nullable: true, // arithmetic / coalesce can produce NULL on bad input
            });
        }
        _ => return None,
    };
    Some(ExprShape {
        name: "?column?".to_string(),
        ty,
        nullable,
    })
}

fn collect_parameter_oids(stmt: &Statement, catalog: &Catalog) -> Vec<u32> {
    let max = max_placeholder(stmt);
    if max == 0 {
        return Vec::new();
    }
    // PG ParameterDescription is one OID per declared $N.
    //
    // v7.37.43-T4 — return TEXT (oid 25) instead of "unknown"
    // (oid 0) for placeholders SPG can't statically type. sqlx-
    // postgres 0.8 treats OID 0 as "user-defined type, fetch
    // metadata from pg_catalog.pg_type", which routes through
    // `maybe_fetch_type_info_by_oid` → `fetch_type_by_oid`'s
    // `SELECT … FROM pg_catalog.pg_type WHERE oid = $1`. That
    // inner query also has a placeholder typed OID 0, recursing
    // through ParameterDescription handling until the calling
    // thread's stack overflows. Affected `sqlx::migrate!()`
    // (every drop-in user) and `sqlx::query("…").bind(…)`
    // (every parameterised SELECT) on the very first Execute.
    //
    // OID 25 is the TEXT built-in. sqlx's `PgTypeInfo::try_from_oid(25)`
    // returns `Some(Text)` synchronously and skips the catalog
    // round-trip. The actual data type comes from the bound
    // value at Execute time; the text wire format makes the
    // type advisory rather than load-bearing.
    //
    // v7.39 (binary results) — but binary-first clients
    // (tokio-postgres, JDBC binary mode) VALIDATE the declared OID
    // against the Rust/Java value they bind, so TEXT-for-everything
    // rejects `WHERE i = $1` with an i32. Infer the real type where
    // the context makes it unambiguous — `col <op> $N` picks the
    // column's type, `INSERT INTO t (…) VALUES ($1, …)` / `UPDATE t
    // SET col = $N` pick the target column — and keep TEXT for
    // anything the walk can't pin (sqx's oid-0 recursion stays
    // fixed because 25 remains the fallback, never 0).
    let mut oids = alloc::vec![25u32; max as usize];
    infer_placeholder_oids(stmt, catalog, &mut oids);
    oids
}

/// v7.39 — PG type OID for a column DataType, for ParameterDescription.
/// Only the families drivers actually bind; anything else keeps the
/// TEXT fallback upstream.
fn wire_oid_for(ty: DataType) -> u32 {
    match ty {
        DataType::Bool => 16,
        DataType::SmallInt => 21,
        DataType::Int => 23,
        DataType::BigInt => 20,
        DataType::Real => 700,
        DataType::Float => 701,
        DataType::Numeric { .. } => 1700,
        DataType::Date => 1082,
        DataType::Time => 1083,
        DataType::Timestamp => 1114,
        DataType::Timestamptz => 1184,
        DataType::Uuid => 2950,
        DataType::Bytes => 17,
        DataType::Json => 114,
        DataType::Jsonb => 3802,
        _ => 25,
    }
}

/// v7.39 — best-effort placeholder typing from column context.
fn infer_placeholder_oids(stmt: &Statement, catalog: &Catalog, oids: &mut [u32]) {
    let col_oid = |schema: &[ColumnSchema], name: &spg_sql::ast::ColumnName| -> Option<u32> {
        schema
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(&name.name))
            .map(|c| wire_oid_for(c.ty))
    };
    let mut mark = |n: u16, oid: Option<u32>| {
        if let Some(oid) = oid
            && let Some(slot) = oids.get_mut((n as usize).saturating_sub(1))
        {
            *slot = oid;
        }
    };
    match stmt {
        Statement::Select(s) => {
            let Some(from) = &s.from else { return };
            if !from.joins.is_empty() {
                return;
            }
            let Some(t) = catalog.get(&from.primary.name) else {
                return;
            };
            let schema = t.schema().columns.clone();
            if let Some(w) = &s.where_ {
                walk_expr(w, &mut |e| {
                    if let Expr::Binary { lhs, rhs, .. } = e {
                        match (lhs.as_ref(), rhs.as_ref()) {
                            (Expr::Column(c), Expr::Placeholder(n))
                            | (Expr::Placeholder(n), Expr::Column(c)) => {
                                mark(*n, col_oid(&schema, c));
                            }
                            _ => {}
                        }
                    }
                });
            }
        }
        Statement::Insert(ins) => {
            let Some(t) = catalog.get(&ins.table) else {
                return;
            };
            let schema = t.schema().columns.clone();
            // Column order: the explicit column list, else table order.
            let order: alloc::vec::Vec<usize> = match &ins.columns {
                Some(cols) if !cols.is_empty() => cols
                    .iter()
                    .map(|name| {
                        schema
                            .iter()
                            .position(|c| c.name.eq_ignore_ascii_case(name))
                            .unwrap_or(usize::MAX)
                    })
                    .collect(),
                _ => (0..schema.len()).collect(),
            };
            for row in &ins.rows {
                for (i, e) in row.iter().enumerate() {
                    if let Expr::Placeholder(n) = e
                        && let Some(&pos) = order.get(i)
                        && let Some(c) = schema.get(pos)
                    {
                        mark(*n, Some(wire_oid_for(c.ty)));
                    }
                }
            }
        }
        Statement::Update(u) => {
            let Some(t) = catalog.get(&u.table) else {
                return;
            };
            let schema = t.schema().columns.clone();
            for (col, e) in &u.assignments {
                if let Expr::Placeholder(n) = e
                    && let Some(c) = schema.iter().find(|c| c.name.eq_ignore_ascii_case(col))
                {
                    mark(*n, Some(wire_oid_for(c.ty)));
                }
            }
            if let Some(w) = &u.where_ {
                walk_expr(w, &mut |e| {
                    if let Expr::Binary { lhs, rhs, .. } = e {
                        match (lhs.as_ref(), rhs.as_ref()) {
                            (Expr::Column(c), Expr::Placeholder(n))
                            | (Expr::Placeholder(n), Expr::Column(c)) => {
                                mark(*n, col_oid(&schema, c));
                            }
                            _ => {}
                        }
                    }
                });
            }
        }
        _ => {}
    }
}

fn max_placeholder(stmt: &Statement) -> u16 {
    let mut max: u16 = 0;
    walk_statement(stmt, &mut |e| {
        if let Expr::Placeholder(n) = e {
            max = max.max(*n);
        }
    });
    max
}

fn walk_statement(stmt: &Statement, f: &mut impl FnMut(&Expr)) {
    match stmt {
        Statement::Select(s) => walk_select(s, f),
        Statement::Insert(s) => {
            for row in &s.rows {
                for e in row {
                    walk_expr(e, f);
                }
            }
        }
        Statement::Update(s) => {
            for (_, e) in &s.assignments {
                walk_expr(e, f);
            }
            if let Some(w) = &s.where_ {
                walk_expr(w, f);
            }
        }
        Statement::Delete(s) => {
            if let Some(w) = &s.where_ {
                walk_expr(w, f);
            }
        }
        // v7.39 (round 225) — the body is a whole Statement (SELECT or DML).
        Statement::Explain(inner) => {
            if let Statement::Select(sel) = &*inner.inner {
                walk_select(sel, f);
            }
        }
        _ => {}
    }
}

fn walk_select(s: &SelectStatement, f: &mut impl FnMut(&Expr)) {
    for item in &s.items {
        if let SelectItem::Expr { expr, .. } = item {
            walk_expr(expr, f);
        }
    }
    if let Some(w) = &s.where_ {
        walk_expr(w, f);
    }
    if let Some(h) = &s.having {
        walk_expr(h, f);
    }
    if let Some(gb) = &s.group_by {
        for e in gb {
            walk_expr(e, f);
        }
    }
    for (_, peer) in &s.unions {
        walk_select(peer, f);
    }
}

fn walk_expr(e: &Expr, f: &mut impl FnMut(&Expr)) {
    f(e);
    match e {
        Expr::NamedArg { expr, .. } => walk_expr(expr, f),
        Expr::Variadic(expr) => walk_expr(expr, f),
        Expr::AggregateOrdered { call, order_by, .. } => {
            walk_expr(call, f);
            for o in order_by {
                walk_expr(&o.expr, f);
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            walk_expr(lhs, f);
            walk_expr(rhs, f);
        }
        Expr::Unary { expr, .. } => walk_expr(expr, f),
        Expr::Cast { expr, .. } | Expr::FieldAccess { base: expr, .. } => walk_expr(expr, f),
        Expr::IsNull { expr, .. } | Expr::BoolTest { expr, .. } => walk_expr(expr, f),
        Expr::Like { expr, pattern, .. } => {
            walk_expr(expr, f);
            walk_expr(pattern, f);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                walk_expr(a, f);
            }
        }
        Expr::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for a in args {
                walk_expr(a, f);
            }
            for p in partition_by {
                walk_expr(p, f);
            }
            for (o, _, _) in order_by {
                walk_expr(o, f);
            }
        }
        Expr::ScalarSubquery(s) => walk_select(s, f),
        Expr::Exists { subquery, .. } => walk_select(subquery, f),
        Expr::InSubquery { expr, subquery, .. } => {
            walk_expr(expr, f);
            walk_select(subquery, f);
        }
        Expr::RowInSubquery { row, subquery, .. } => {
            for el in row {
                walk_expr(el, f);
            }
            walk_select(subquery, f);
        }
        Expr::RowCmpSubquery { row, subquery, .. } => {
            for el in row {
                walk_expr(el, f);
            }
            walk_select(subquery, f);
        }
        Expr::Extract { source, .. } => walk_expr(source, f),
        Expr::Array(items) => {
            for elem in items {
                walk_expr(elem, f);
            }
        }
        Expr::ArraySubscript { target, index } => {
            walk_expr(target, f);
            walk_expr(index, f);
        }
        Expr::ArraySlice { target, lo, hi } => {
            walk_expr(target, f);
            if let Some(l) = lo {
                walk_expr(l, f);
            }
            if let Some(h) = hi {
                walk_expr(h, f);
            }
        }
        Expr::AnyAll { expr, array, .. } => {
            walk_expr(expr, f);
            walk_expr(array, f);
        }
        Expr::InList { expr, list, .. } => {
            walk_expr(expr, f);
            for item in list {
                walk_expr(item, f);
            }
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(o) = operand {
                walk_expr(o, f);
            }
            for (w, t) in branches {
                walk_expr(w, f);
                walk_expr(t, f);
            }
            if let Some(e) = else_branch {
                walk_expr(e, f);
            }
        }
        Expr::Literal(_) | Expr::Column(_) | Expr::Placeholder(_) => {}
    }
}

/// v7.39 (round 310, V31) — a TIMESTAMPTZ array has to be recognised from
/// the element EXPRESSIONS, not from their values.
///
/// `Value::Timestamp` is the runtime form of both timestamp types — the
/// zone-ness rides on the static type, exactly as enum-ness and
/// composite-ness do (rounds 54 / 56). An array builder picks its variant
/// by looking at what it materialised, so it could only ever answer
/// `timestamp without time zone[]`; the array then would not go into a
/// `timestamptz[]` column and rendered without its offset.
///
/// Shared because there are two array builders — the evaluator's and the
/// literal-folding one INSERT VALUES uses. Fixing only the first left the
/// INSERT still rejecting its own well-typed array.
///
/// Only a uniformly-timestamptz constructor is upgraded. A mixed one has
/// already been unified by the caller, and if that settled on a plain
/// timestamp then plain is what PG resolves it to.
pub(crate) fn upgrade_timestamptz_array(
    v: Value<'static>,
    items: &[Expr],
    columns: &[ColumnSchema],
) -> Value<'static> {
    let Value::TimestampArray(elems) = v else {
        return v;
    };
    if items.is_empty()
        || !items
            .iter()
            .all(|e| describe_expr(e, columns).is_some_and(|s| s.ty == DataType::Timestamptz))
    {
        return Value::TimestampArray(elems);
    }
    Value::TimestamptzArray(elems)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Engine;
    use spg_sql::parser::parse_statement;

    fn parse(sql: &str) -> Statement {
        parse_statement(sql).expect("parses")
    }

    #[test]
    fn describe_returns_columns_for_wildcard_select() {
        let mut eng = Engine::new();
        eng.execute("CREATE TABLE t (a INT, b TEXT)").unwrap();
        let stmt = eng.prepare("SELECT * FROM t").unwrap();
        let (params, cols) = describe_prepared(&stmt, eng_catalog(&eng));
        assert_eq!(params, Vec::<u32>::new());
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0].name, "a");
        assert_eq!(cols[0].ty, DataType::Int);
        assert_eq!(cols[1].name, "b");
        assert_eq!(cols[1].ty, DataType::Text);
    }

    #[test]
    fn describe_returns_columns_for_projection_select() {
        let mut eng = Engine::new();
        eng.execute("CREATE TABLE t (a INT, b TEXT)").unwrap();
        let stmt = eng.prepare("SELECT b, a FROM t").unwrap();
        let (_, cols) = describe_prepared(&stmt, eng_catalog(&eng));
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0].name, "b");
        assert_eq!(cols[0].ty, DataType::Text);
        assert_eq!(cols[1].name, "a");
        assert_eq!(cols[1].ty, DataType::Int);
    }

    #[test]
    fn describe_counts_placeholders() {
        let stmt = parse("SELECT * FROM t WHERE id = $1 AND name = $2");
        let (params, _) = describe_prepared(&stmt, &Catalog::new());
        // v7.37.43-T4 — placeholders report OID 25 (TEXT) instead of
        // OID 0 ("unknown") because sqlx-postgres 0.8 routes OID 0
        // through a recursive pg_catalog.pg_type fetch that
        // stack-overflows the caller; 25 hits the synchronous
        // PgTypeInfo::try_from_oid fast path. See `collect_parameter_oids`
        // for the full rationale.
        assert_eq!(params, alloc::vec![25u32, 25u32]);
    }

    #[test]
    fn describe_resolves_a_join_namespace() {
        // v7.39 (round 462) — this used to assert the opposite: a JOIN
        // fell through to NoData "which drivers tolerate". They do not.
        // Execute owes no RowDescription, so NoData here left every
        // extended-protocol client holding rows with no column metadata.
        // PG18 describes the same statement as four columns.
        let mut eng = Engine::new();
        eng.execute("CREATE TABLE a (id INT)").unwrap();
        eng.execute("CREATE TABLE b (id INT)").unwrap();
        let stmt = eng
            .prepare("SELECT * FROM a JOIN b ON a.id = b.id")
            .unwrap();
        let (_, cols) = describe_prepared(&stmt, eng_catalog(&eng));
        let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
        // PG labels a join's `*` by the BARE column names, duplicates and
        // all — measured on PG18: `id | id`.
        assert_eq!(names, alloc::vec!["id", "id"]);
    }

    #[test]
    fn describe_emits_empty_columns_for_non_select() {
        let stmt = parse("INSERT INTO t VALUES (1)");
        let (params, cols) = describe_prepared(&stmt, &Catalog::new());
        assert_eq!(params, Vec::<u32>::new());
        assert!(cols.is_empty());
    }

    fn eng_catalog(eng: &Engine) -> &Catalog {
        eng.catalog()
    }
}
