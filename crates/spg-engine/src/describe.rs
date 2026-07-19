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
//! - `output_columns`: for `SELECT` against a single (non-JOIN)
//!   FROM clause, resolve each `SELECT` item to a column shape via
//!   the existing catalog lookups. For everything else (JOIN,
//!   subquery, non-SELECT, INSERT, UPDATE, DELETE without
//!   RETURNING — and SPG has no RETURNING yet) return an empty Vec
//!   which the pgwire layer maps to a `NoData` reply.
//!
//! This covers what JDBC / sqlx / pgx / psycopg3 actually call
//! Describe on: plain `SELECT col1, col2 FROM t WHERE …` shapes.
//! Complex shapes degrade to NoData, which drivers tolerate.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use spg_sql::ast::{Expr, Literal, SelectItem, SelectStatement, Statement, UnOp};
use spg_storage::{Catalog, ColumnSchema, DataType};

/// One-shot describe of a prepared `Statement`.
///
/// Returns `(parameter_oids, output_columns)`. Empty `output_columns`
/// means "no row description available" → pgwire sends NoData.
pub fn describe_prepared(stmt: &Statement, catalog: &Catalog) -> (Vec<u32>, Vec<ColumnSchema>) {
    let params = collect_parameter_oids(stmt, catalog);
    let columns = describe_output_columns(stmt, catalog);
    (params, columns)
}

fn describe_output_columns(stmt: &Statement, catalog: &Catalog) -> Vec<ColumnSchema> {
    let Statement::Select(s) = stmt else {
        return Vec::new();
    };
    // Multi-arm UNION falls through to NoData (drivers tolerate).
    if !s.unions.is_empty() {
        return Vec::new();
    }
    // No FROM (`SELECT 1::INT AS one`) → describe items against an
    // empty schema; literal / cast / function items still resolve.
    let Some(from) = &s.from else {
        return describe_select_items(&s.items, &[]);
    };
    // JOIN / subquery FROM falls through to NoData.
    if !from.joins.is_empty() {
        return Vec::new();
    }
    let Some(table) = catalog.get(&from.primary.name) else {
        return Vec::new();
    };
    let schema_cols = &table.schema().columns;
    describe_select_items(&s.items, schema_cols)
}

fn describe_select_items(items: &[SelectItem], schema_cols: &[ColumnSchema]) -> Vec<ColumnSchema> {
    let mut out: Vec<ColumnSchema> = Vec::with_capacity(items.len());
    for item in items {
        match item {
            // A qualified wildcard describes the same as `*` here (single-table
            // describe); a joined describe degrades to NoData elsewhere anyway.
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                for c in schema_cols {
                    out.push(c.clone());
                }
            }
            SelectItem::Expr { expr, alias } => {
                let Some(desc) = describe_expr(expr, schema_cols) else {
                    return Vec::new();
                };
                let name = alias.clone().unwrap_or(desc.name);
                out.push(ColumnSchema {
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
    // A view chain deep enough to hit this is either pathological or a
    // cycle the catalog should not contain; stop rather than recurse.
    if depth > 16 {
        return Vec::new();
    }
    let Some(view) = catalog.views().get(view_name) else {
        return Vec::new();
    };
    let Ok(Statement::Select(select)) = spg_sql::parser::parse_statement(&view.body) else {
        return Vec::new();
    };
    let mut ns: Vec<ColumnSchema> = Vec::new();
    if let Some(from) = &select.from {
        let mut add = |name: &str| {
            if let Some(t) = catalog.get(name) {
                ns.extend(t.schema().columns.iter().cloned());
            } else {
                ns.extend(describe_view_columns_depth(catalog, name, depth + 1));
            }
        };
        add(&from.primary.name);
        for j in &from.joins {
            add(&j.table.name);
        }
    }
    let mut out = describe_select_items(&select.items, &ns);
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
        Expr::FunctionCall { name, args } => function_return_shape(name, args, schema_cols),
        // v7.26 (round-20 C) — aggregate modifiers delegate to the
        // inner call (DISTINCT / internal ORDER BY don't change the
        // output type).
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
                _ => {
                    let inner = describe_expr(lhs, schema_cols)?;
                    // v7.38 (read01 A-bitcat) — PG's `||` on bit strings is
                    // `bitcat`, whose result is always `bit varying`: the
                    // operands widen to varbit and the concatenated length
                    // isn't a fixed `bit(N)`. So `B'10' || B'11'` is
                    // `bit varying`, matching PG's pg_typeof — not the left
                    // operand's `bit`.
                    let ty = if matches!(op, B::Concat)
                        && matches!(inner.ty, DataType::Bit | DataType::BitVarying)
                    {
                        DataType::BitVarying
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
        "current_time" | "localtime" => (DataType::Timestamp, false), // approx — SPG lacks TIME
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
            let ty = match elem {
                Some(DataType::Int | DataType::SmallInt) => DataType::IntArray,
                Some(DataType::BigInt) => DataType::BigIntArray,
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
        "max" | "min" | "abs" | "floor" | "ceil" | "ceiling" | "round"
        | "trunc" | "mod" | "power" | "pow" | "sqrt" | "sign" => {
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
        Expr::IsNull { expr, .. } => walk_expr(expr, f),
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
    fn describe_emits_empty_columns_for_join() {
        let mut eng = Engine::new();
        eng.execute("CREATE TABLE a (id INT)").unwrap();
        eng.execute("CREATE TABLE b (id INT)").unwrap();
        let stmt = eng
            .prepare("SELECT * FROM a JOIN b ON a.id = b.id")
            .unwrap();
        let (_, cols) = describe_prepared(&stmt, eng_catalog(&eng));
        // JOIN shape falls through to NoData → empty Vec.
        assert!(cols.is_empty());
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
