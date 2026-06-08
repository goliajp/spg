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

use spg_sql::ast::{Expr, SelectItem, SelectStatement, Statement, UnOp};
use spg_storage::{Catalog, ColumnSchema, DataType};

/// One-shot describe of a prepared `Statement`.
///
/// Returns `(parameter_oids, output_columns)`. Empty `output_columns`
/// means "no row description available" → pgwire sends NoData.
pub fn describe_prepared(stmt: &Statement, catalog: &Catalog) -> (Vec<u32>, Vec<ColumnSchema>) {
    let params = collect_parameter_oids(stmt);
    let columns = describe_output_columns(stmt, catalog);
    (params, columns)
}

fn describe_output_columns(stmt: &Statement, catalog: &Catalog) -> Vec<ColumnSchema> {
    let Statement::Select(s) = stmt else {
        return Vec::new();
    };
    // Only handle single-table FROM. JOIN + subquery + multi-arm UNION
    // fall through to NoData (drivers tolerate this).
    let Some(from) = &s.from else {
        return Vec::new();
    };
    if !from.joins.is_empty() {
        return Vec::new();
    }
    if !s.unions.is_empty() {
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
            SelectItem::Wildcard => {
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
                });
            }
        }
    }
    out
}

pub(crate) struct ExprShape {
    pub(crate) name: String,
    pub(crate) ty: DataType,
    pub(crate) nullable: bool,
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
        Expr::FunctionCall { name, args } => {
            function_return_shape(name, args, schema_cols)
        }
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
        "now" | "current_timestamp" | "localtimestamp" | "transaction_timestamp"
        | "statement_timestamp" | "clock_timestamp" => (DataType::Timestamptz, false),
        "current_date" => (DataType::Date, false),
        "current_time" | "localtime" => (DataType::Timestamp, false), // approx — SPG lacks TIME
        // Text-returning library — every fn that produces a string.
        "concat" | "concat_ws" | "format" | "lower" | "upper" | "trim"
        | "ltrim" | "rtrim" | "substring" | "substr" | "replace"
        | "split_part" | "repeat" | "lpad" | "rpad" | "left" | "right"
        | "translate" | "regexp_replace" | "to_char" | "encode"
        | "host" | "network" | "version" | "database" | "current_database"
        | "current_schema" | "current_user"
        | "session_user" | "user" | "pg_get_serial_sequence"
        | "pg_get_constraintdef" | "pg_get_indexdef"
        | "date_format" | "pg_typeof" => (DataType::Text, true),
        // Bytes-returning.
        "decode" | "hex" => (DataType::Bytes, true),
        // Integer-returning length / position helpers.
        "length" | "char_length" | "character_length" | "octet_length"
        | "bit_length" | "position" | "strpos" | "ascii" | "masklen" => (DataType::Int, true),
        // BigInt-returning.
        "count" | "count_star" | "nextval" | "currval" | "lastval"
        | "unix_timestamp" => (DataType::BigInt, true),
        // Float / double-precision returns.
        "random" | "ts_rank" | "ts_rank_cd" | "similarity" | "ln"
        | "log" | "log2" | "exp" | "sin" | "cos" | "tan" | "asin"
        | "acos" | "atan" | "atan2" | "degrees" | "radians" | "pi" => (DataType::Float, true),
        // Boolean predicate-returning.
        "starts_with" => (DataType::Bool, true),
        // Arrays.
        "regexp_matches" | "regexp_split_to_array" | "show_trgm"
        | "string_to_array" | "array_remove" | "array_append"
        | "array_cat" => (DataType::TextArray, true),
        // JSON.
        "to_json" | "to_jsonb" | "json_build_object" | "jsonb_build_object"
        | "json_build_array" | "jsonb_build_array" | "json_object"
        | "jsonb_object" | "jsonb_set" | "jsonb_insert"
        | "jsonb_path_query" | "jsonb_path_query_first"
        | "jsonb_path_query_array" | "json_path_query" => (DataType::Json, true),
        // FTS types.
        "to_tsvector" => (DataType::TsVector, true),
        "to_tsquery" | "plainto_tsquery" | "phraseto_tsquery"
        | "websearch_to_tsquery" => (DataType::TsQuery, true),
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
        "date_trunc" | "make_timestamp" => (DataType::Timestamp, true),
        "from_unixtime" => {
            if args.len() >= 2 {
                (DataType::Text, true)
            } else {
                (DataType::Timestamp, true)
            }
        }
        "make_date" | "to_date" => (DataType::Date, true),
        "date_part" | "extract" => (DataType::Float, true),
        // Pass-through aggregates / conditionals: derive the type
        // from the first arg.
        "sum" | "avg" | "max" | "min" | "abs" | "floor" | "ceil"
        | "ceiling" | "round" | "trunc" | "mod" | "power" | "pow"
        | "sqrt" | "sign" | "coalesce" | "nullif" | "greatest"
        | "least" | "ifnull" | "isnull" => {
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

fn collect_parameter_oids(stmt: &Statement) -> Vec<u32> {
    let max = max_placeholder(stmt);
    if max == 0 {
        return Vec::new();
    }
    // PG ParameterDescription is one OID per declared $N. We don't
    // infer types, so report 0 ("unknown — bind-time inference").
    alloc::vec![0u32; max as usize]
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
        Statement::Explain(inner) => walk_select(&inner.inner, f),
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
        Expr::Binary { lhs, rhs, .. } => {
            walk_expr(lhs, f);
            walk_expr(rhs, f);
        }
        Expr::Unary { expr, .. } => walk_expr(expr, f),
        Expr::Cast { expr, .. } => walk_expr(expr, f),
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
            for (o, _) in order_by {
                walk_expr(o, f);
            }
        }
        Expr::ScalarSubquery(s) => walk_select(s, f),
        Expr::Exists { subquery, .. } => walk_select(subquery, f),
        Expr::InSubquery { expr, subquery, .. } => {
            walk_expr(expr, f);
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
        Expr::AnyAll { expr, array, .. } => {
            walk_expr(expr, f);
            walk_expr(array, f);
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
        assert_eq!(params, alloc::vec![0u32, 0u32]);
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
