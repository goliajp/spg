//! System-catalog view synthesis — `information_schema.*`, `pg_catalog.*`,
//! and `mysql.*` metadata tables materialised on demand. Split out of
//! `lib.rs` (v7.32 engine modularisation). Each `synth_*` maps the live
//! catalog (or Engine state, for roles/settings/users) to a
//! `(schema, rows)` pair that `materialise_meta_view` installs.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use spg_sql::ast::SelectStatement;
use spg_storage::{Catalog, ColumnSchema, DataType, Row, TableSchema, Value};

use crate::{Engine, EngineError};

/// v7.16.2 — map an SPG [`DataType`] to the PG-canonical
/// `information_schema.columns.data_type` text. Covers the
/// values mailrs's migrations probe (`'ARRAY'`, `'integer'`,
/// `'text'`, …). Unknown variants fall back to the SPG name
/// downcased — better than panicking on a future DataType.
pub(crate) fn pg_data_type_text(ty: DataType) -> alloc::string::String {
    let s = match ty {
        DataType::Int => "integer",
        DataType::BigInt => "bigint",
        DataType::SmallInt => "smallint",
        DataType::Float => "double precision",
        DataType::Bool => "boolean",
        DataType::Text => "text",
        DataType::Varchar(_) => "character varying",
        DataType::Date => "date",
        DataType::Timestamp => "timestamp without time zone",
        DataType::Timestamptz => "timestamp with time zone",
        DataType::Json => "jsonb",
        DataType::Bytes => "bytea",
        DataType::TextArray | DataType::IntArray | DataType::BigIntArray => "ARRAY",
        DataType::TsVector => "tsvector",
        DataType::TsQuery => "tsquery",
        DataType::Vector { .. } => "USER-DEFINED",
        // Non-exhaustive — fall back to "USER-DEFINED" the way
        // PG labels any pg_type it doesn't recognise.
        _ => "USER-DEFINED",
    };
    alloc::string::String::from(s)
}

/// v7.16.2 — synthesise `information_schema.columns`. mailrs
/// queries are of shape `SELECT 1 FROM information_schema.columns
/// WHERE table_name = … AND column_name = … AND data_type = …` —
/// the v7.16.2 view returns the columns mailrs probes; broader
/// PG-spec parity (ordinal_position, is_nullable, character_
/// maximum_length, udt_name, …) lands as needed.
pub(crate) fn synth_information_schema_columns(
    cat: &Catalog,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("table_catalog", DataType::Text, false),
        ColumnSchema::new("table_schema", DataType::Text, false),
        ColumnSchema::new("table_name", DataType::Text, false),
        ColumnSchema::new("column_name", DataType::Text, false),
        ColumnSchema::new("ordinal_position", DataType::Int, false),
        ColumnSchema::new("is_nullable", DataType::Text, false),
        ColumnSchema::new("data_type", DataType::Text, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for tname in cat.table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        for (i, col) in t.schema().columns.iter().enumerate() {
            #[allow(clippy::cast_possible_wrap)]
            let ordinal = (i + 1) as i32;
            rows.push(Row::new(alloc::vec![
                Value::text("spg"),
                Value::text("public"),
                Value::text(tname.clone()),
                Value::text(col.name.clone()),
                Value::Int(ordinal),
                Value::text::<&str>(if col.nullable { "YES" } else { "NO" }),
                Value::text(pg_data_type_text(col.ty)),
            ]));
        }
    }
    (schema, rows)
}

/// v7.16.2 — synthesise `information_schema.tables`.
pub(crate) fn synth_information_schema_tables(
    cat: &Catalog,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("table_catalog", DataType::Text, false),
        ColumnSchema::new("table_schema", DataType::Text, false),
        ColumnSchema::new("table_name", DataType::Text, false),
        ColumnSchema::new("table_type", DataType::Text, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for tname in cat.table_names() {
        rows.push(Row::new(alloc::vec![
            Value::text("spg"),
            Value::text("public"),
            Value::text(tname.clone()),
            Value::text("BASE TABLE"),
        ]));
    }
    (schema, rows)
}

/// v7.16.2 — synthesise `pg_catalog.pg_class`. Minimum shape
/// for psql `\d` / ORM probes: `relname` + `relkind`. Each
/// user table emits one row.
pub(crate) fn synth_pg_class(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("relname", DataType::Text, false),
        ColumnSchema::new("relkind", DataType::Text, false),
        ColumnSchema::new("relnamespace", DataType::BigInt, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for tname in cat.table_names() {
        rows.push(Row::new(alloc::vec![
            Value::text(tname.clone()),
            Value::text("r"),
            Value::BigInt(2200), // PG's `public` namespace OID
        ]));
    }
    (schema, rows)
}

/// v7.16.2 — synthesise `pg_catalog.pg_attribute`. Minimum
/// shape: `attrelid` (text — SPG has no OID), `attname`,
/// `attnum`, `atttypid` (text), `attnotnull`.
pub(crate) fn synth_pg_attribute(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("attrelid", DataType::Text, false),
        ColumnSchema::new("attname", DataType::Text, false),
        ColumnSchema::new("attnum", DataType::Int, false),
        ColumnSchema::new("atttypid", DataType::Text, false),
        ColumnSchema::new("attnotnull", DataType::Bool, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for tname in cat.table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        for (i, col) in t.schema().columns.iter().enumerate() {
            #[allow(clippy::cast_possible_wrap)]
            let ordinal = (i + 1) as i32;
            rows.push(Row::new(alloc::vec![
                Value::text(tname.clone()),
                Value::text(col.name.clone()),
                Value::Int(ordinal),
                Value::text(pg_data_type_text(col.ty)),
                Value::Bool(!col.nullable),
            ]));
        }
    }
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-50 — synthesise `pg_catalog.pg_type`. The
/// returned rows cover every built-in scalar / array type sqlx,
/// SQLAlchemy, Diesel and pgAdmin look up at compile / connect
/// time. PG-canonical schema columns we expose:
///   * oid           — type OID (the lookup key sqlx uses)
///   * typname       — canonical type name (`int4`, `text`, …)
///   * typlen        — width in bytes (-1 for var-length)
///   * typtype       — `b`ase / `c`omposite / `e`num / etc.
///   * typcategory   — PG type category single-char
///   * typelem       — element OID for arrays (0 otherwise)
///   * typarray      — array-type OID (0 if no array type)
///   * typnamespace  — schema OID (always `public` = 2200)
///
/// Other pg_type columns (typowner, typinput/typoutput, etc.)
/// land in follow-up work — sqlx encoders don't query them at
/// connect time.
pub(crate) fn synth_pg_type(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("typname", DataType::Text, false),
        ColumnSchema::new("typlen", DataType::SmallInt, false),
        ColumnSchema::new("typtype", DataType::Text, false),
        ColumnSchema::new("typcategory", DataType::Text, false),
        ColumnSchema::new("typelem", DataType::BigInt, false),
        ColumnSchema::new("typarray", DataType::BigInt, false),
        ColumnSchema::new("typnamespace", DataType::BigInt, false),
    ];
    // (oid, name, len, type, cat, elem, array_oid). PG OID
    // numbers come straight from `pg_type.dat`.
    let scalars: &[(i64, &str, i16, &str, &str, i64, i64)] = &[
        // bool
        (16, "bool", 1, "b", "B", 0, 1000),
        (17, "bytea", -1, "b", "U", 0, 1001),
        (18, "char", 1, "b", "S", 0, 1002),
        (19, "name", 64, "b", "S", 0, 1003),
        (20, "int8", 8, "b", "N", 0, 1016),
        (21, "int2", 2, "b", "N", 0, 1005),
        (23, "int4", 4, "b", "N", 0, 1007),
        (24, "regproc", 4, "b", "N", 0, 1008),
        (25, "text", -1, "b", "S", 0, 1009),
        (26, "oid", 4, "b", "N", 0, 1028),
        (114, "json", -1, "b", "U", 0, 199),
        (142, "xml", -1, "b", "U", 0, 143),
        (700, "float4", 4, "b", "N", 0, 1021),
        (701, "float8", 8, "b", "N", 0, 1022),
        (650, "cidr", -1, "b", "I", 0, 651),
        (869, "inet", -1, "b", "I", 0, 1041),
        (829, "macaddr", 6, "b", "U", 0, 1040),
        (1042, "bpchar", -1, "b", "S", 0, 1014),
        (1043, "varchar", -1, "b", "S", 0, 1015),
        (1082, "date", 4, "b", "D", 0, 1182),
        (1083, "time", 8, "b", "D", 0, 1183),
        (1114, "timestamp", 8, "b", "D", 0, 1115),
        (1184, "timestamptz", 8, "b", "D", 0, 1185),
        (1186, "interval", 16, "b", "T", 0, 1187),
        (1266, "timetz", 12, "b", "D", 0, 1270),
        (1700, "numeric", -1, "b", "N", 0, 1231),
        (790, "money", 8, "b", "N", 0, 791),
        (2950, "uuid", 16, "b", "U", 0, 2951),
        (3802, "jsonb", -1, "b", "U", 0, 3807),
        (3614, "tsvector", -1, "b", "U", 0, 3643),
        (3615, "tsquery", -1, "b", "U", 0, 3645),
        // hstore + range types — typcategory 'U' (user) / 'R' (range).
        (3908, "tstzrange", -1, "r", "R", 0, 3909),
        (3910, "tsrange", -1, "r", "R", 0, 3911),
        (3904, "int4range", -1, "r", "R", 0, 3905),
        (3926, "int8range", -1, "r", "R", 0, 3927),
        (3906, "numrange", -1, "r", "R", 0, 3907),
        (3912, "daterange", -1, "r", "R", 0, 3913),
    ];
    // Array companion types share the typelem / typcategory='A'.
    // We emit just the array OIDs the scalars reference.
    let arrays: &[(i64, &str, i64)] = &[
        (1000, "_bool", 16),
        (1001, "_bytea", 17),
        (1002, "_char", 18),
        (1003, "_name", 19),
        (1016, "_int8", 20),
        (1005, "_int2", 21),
        (1007, "_int4", 23),
        (1008, "_regproc", 24),
        (1009, "_text", 25),
        (1028, "_oid", 26),
        (199, "_json", 114),
        (143, "_xml", 142),
        (1021, "_float4", 700),
        (1022, "_float8", 701),
        (651, "_cidr", 650),
        (1041, "_inet", 869),
        (1040, "_macaddr", 829),
        (1014, "_bpchar", 1042),
        (1015, "_varchar", 1043),
        (1182, "_date", 1082),
        (1183, "_time", 1083),
        (1115, "_timestamp", 1114),
        (1185, "_timestamptz", 1184),
        (1187, "_interval", 1186),
        (1270, "_timetz", 1266),
        (1231, "_numeric", 1700),
        (791, "_money", 790),
        (2951, "_uuid", 2950),
        (3807, "_jsonb", 3802),
        (3643, "_tsvector", 3614),
        (3645, "_tsquery", 3615),
    ];
    let mut rows: Vec<Row<'static>> = Vec::with_capacity(scalars.len() + arrays.len());
    for &(oid, name, len, ty, cat, elem, arr) in scalars {
        rows.push(Row::new(alloc::vec![
            Value::BigInt(oid),
            Value::text::<String>(name.into()),
            Value::SmallInt(len),
            Value::text::<String>(ty.into()),
            Value::text::<String>(cat.into()),
            Value::BigInt(elem),
            Value::BigInt(arr),
            Value::BigInt(2200),
        ]));
    }
    for &(oid, name, elem) in arrays {
        rows.push(Row::new(alloc::vec![
            Value::BigInt(oid),
            Value::text::<String>(name.into()),
            Value::SmallInt(-1),
            Value::text::<String>("b".into()),
            Value::text::<String>("A".into()),
            Value::BigInt(elem),
            Value::BigInt(0),
            Value::BigInt(2200),
        ]));
    }
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-51 — synthesise `pg_catalog.pg_proc`. ORM /
/// pgAdmin probes look up functions by name; SPG synthesises rows
/// for the built-in scalar functions / aggregates / window funcs
/// the engine actually dispatches. SPG has no user-defined
/// functions yet so the table is a stable static list.
///
/// Schema columns exposed:
///   * oid (BigInt) — function OID from PG's pg_proc.dat
///   * proname (Text) — function name (lowercase)
///   * pronamespace (BigInt) — 11 (`pg_catalog`)
///   * prokind (Text) — 'f' function, 'a' aggregate, 'w' window
///   * pronargs (SmallInt) — declared arg count (-1 for variadic)
///   * prorettype (BigInt) — return type OID (matches synth_pg_type)
/// v7.24 (round-16 D) — synthesise `pg_catalog.pg_trigger` from the
/// live catalog. PG-shaped core columns (tgname, tgenabled with
/// 'O'/'D') plus pragmatic text columns PG keeps relational
/// (relname, timing, events, function) so health checks don't need
/// oid joins.
pub(crate) fn synth_pg_trigger(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("tgname", DataType::Text, false),
        ColumnSchema::new("relname", DataType::Text, false),
        ColumnSchema::new("tgenabled", DataType::Text, false),
        ColumnSchema::new("timing", DataType::Text, false),
        ColumnSchema::new("events", DataType::Text, false),
        ColumnSchema::new("function", DataType::Text, false),
    ];
    let rows: Vec<Row<'static>> = cat
        .triggers()
        .iter()
        .map(|t| {
            Row::new(alloc::vec![
                Value::text(t.name.clone()),
                Value::text(t.table.clone()),
                Value::text(if t.enabled { "O" } else { "D" }),
                Value::text(t.timing.clone()),
                Value::text(t.events.join(" OR ")),
                Value::text(t.function.clone()),
            ])
        })
        .collect();
    (schema, rows)
}

pub(crate) fn synth_pg_proc(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("proname", DataType::Text, false),
        ColumnSchema::new("pronamespace", DataType::BigInt, false),
        ColumnSchema::new("prokind", DataType::Text, false),
        ColumnSchema::new("pronargs", DataType::Int, false),
        ColumnSchema::new("prorettype", DataType::BigInt, false),
    ];
    // (oid, name, kind, nargs, rettype). OIDs taken from PG's
    // pg_proc.dat for the common subset.
    let funcs: &[(i64, &str, &str, i32, i64)] = &[
        // Scalar functions.
        (1318, "length", "f", 1, 23),
        (871, "upper", "f", 1, 25),
        (870, "lower", "f", 1, 25),
        (936, "substring", "f", 3, 25),
        (937, "substring", "f", 2, 25),
        (3055, "btrim", "f", 1, 25),
        (885, "btrim", "f", 2, 25),
        (3056, "ltrim", "f", 1, 25),
        (875, "ltrim", "f", 2, 25),
        (3057, "rtrim", "f", 1, 25),
        (876, "rtrim", "f", 2, 25),
        (1397, "abs", "f", 1, 23),
        (1396, "abs", "f", 1, 20),
        (1606, "round", "f", 1, 1700),
        (1707, "round", "f", 2, 1700),
        (2308, "ceil", "f", 1, 701),
        (2309, "ceiling", "f", 1, 701),
        (2310, "floor", "f", 1, 701),
        (1376, "sqrt", "f", 1, 701),
        (1369, "ln", "f", 1, 701),
        (1373, "exp", "f", 1, 701),
        (1368, "power", "f", 2, 701),
        (2228, "random", "f", 0, 701),
        // Date / time.
        (1299, "now", "f", 0, 1184),
        (1274, "current_timestamp", "f", 0, 1184),
        (1140, "current_date", "f", 0, 1082),
        (2050, "current_time", "f", 0, 1083),
        (1158, "date_trunc", "f", 2, 1184),
        (1171, "date_part", "f", 2, 701),
        (1172, "age", "f", 1, 1186),
        (936, "to_char", "f", 2, 25),
        // Session / introspection.
        (861, "current_database", "f", 0, 19),
        (745, "current_user", "f", 0, 19),
        (745, "session_user", "f", 0, 19),
        (1402, "current_schema", "f", 0, 19),
        // String concat / format.
        (3058, "concat", "f", -1, 25),
        (3059, "concat_ws", "f", -1, 25),
        (3539, "format", "f", -1, 25),
        // Type introspection.
        (2877, "pg_typeof", "f", 1, 2206),
        // JSON.
        (3198, "json_build_object", "f", -1, 114),
        (3199, "jsonb_build_object", "f", -1, 3802),
        (3271, "json_build_array", "f", -1, 114),
        (3272, "jsonb_build_array", "f", -1, 3802),
        // UUID.
        (3253, "gen_random_uuid", "f", 0, 2950),
        (3252, "uuid_generate_v4", "f", 0, 2950),
        // Aggregates.
        (2147, "count", "a", 0, 20),
        (2803, "count", "a", -1, 20),
        (2116, "max", "a", 1, 23),
        (2132, "min", "a", 1, 23),
        (2108, "sum", "a", 1, 20),
        (2100, "avg", "a", 1, 1700),
        (2517, "string_agg", "a", 2, 25),
        (2747, "array_agg", "a", 1, 1009),
        (2517, "bool_and", "a", 1, 16),
        (2518, "bool_or", "a", 1, 16),
        (2519, "every", "a", 1, 16),
        // Window functions.
        (3100, "row_number", "w", 0, 20),
        (3101, "rank", "w", 0, 20),
        (3102, "dense_rank", "w", 0, 20),
        (3103, "percent_rank", "w", 0, 701),
        (3104, "cume_dist", "w", 0, 701),
        (3105, "lag", "w", -1, 2283),
        (3106, "lead", "w", -1, 2283),
        (3107, "first_value", "w", 1, 2283),
        (3108, "last_value", "w", 1, 2283),
        (3109, "nth_value", "w", 2, 2283),
    ];
    let mut rows: Vec<Row<'static>> = Vec::with_capacity(funcs.len());
    for &(oid, name, kind, nargs, rettype) in funcs {
        rows.push(Row::new(alloc::vec![
            Value::BigInt(oid),
            Value::text::<String>(name.into()),
            Value::BigInt(11),
            Value::text::<String>(kind.into()),
            Value::Int(nargs),
            Value::BigInt(rettype),
        ]));
    }
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-65 — synthesise `mysql.user`. MySQL admin
/// queries (`SELECT user, host FROM mysql.user`) probe this at
/// connect time to list accounts. SPG ships one row per
/// UserStore entry plus a synthetic `root` superuser row for
/// MySQL bootstrap compat.
pub(crate) fn synth_mysql_user(engine: &Engine) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("user", DataType::Text, false),
        ColumnSchema::new("host", DataType::Text, false),
        ColumnSchema::new("select_priv", DataType::Text, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    rows.push(Row::new(alloc::vec![
        Value::text("root"),
        Value::text("localhost"),
        Value::text("Y"),
    ]));
    for (name, _) in engine.users.iter() {
        if name != "root" {
            rows.push(Row::new(alloc::vec![
                Value::text(name.to_string()),
                Value::text::<String>("%".into()),
                Value::text::<String>("Y".into()),
            ]));
        }
    }
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-65 — synthesise `mysql.db`. The
/// per-database privileges table. SPG is single-database so the
/// table surfaces one row per declared user with full privileges
/// on the canonical `postgres` database.
pub(crate) fn synth_mysql_db() -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("host", DataType::Text, false),
        ColumnSchema::new("db", DataType::Text, false),
        ColumnSchema::new("user", DataType::Text, false),
        ColumnSchema::new("select_priv", DataType::Text, false),
    ];
    let rows = alloc::vec![Row::new(alloc::vec![
        Value::text("localhost"),
        Value::text("postgres"),
        Value::text("root"),
        Value::text::<String>("Y".into()),
    ])];
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-63 — synthesise
/// `information_schema.KEY_COLUMN_USAGE`. ORM migration tools
/// (Alembic, Sequelize, TypeORM) walk this view to discover FK
/// relationships in MySQL-flavoured introspection queries.
///
/// Schema columns exposed:
///   * CONSTRAINT_NAME (Text)
///   * TABLE_NAME (Text)
///   * COLUMN_NAME (Text)
///   * ORDINAL_POSITION (Int)
///   * REFERENCED_TABLE_NAME (Text) — empty for non-FK rows
///   * REFERENCED_COLUMN_NAME (Text) — empty for non-FK rows
pub(crate) fn synth_info_key_column_usage(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("constraint_name", DataType::Text, false),
        ColumnSchema::new("table_name", DataType::Text, false),
        ColumnSchema::new("column_name", DataType::Text, false),
        ColumnSchema::new("ordinal_position", DataType::Int, false),
        ColumnSchema::new("referenced_table_name", DataType::Text, false),
        ColumnSchema::new("referenced_column_name", DataType::Text, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for tname in cat.table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        let cols = &t.schema().columns;
        let col_name_at = |pos: usize| -> String {
            cols.get(pos)
                .map_or_else(|| alloc::format!("col{pos}"), |c| c.name.clone())
        };
        // FKs.
        for (fi, fk) in t.schema().foreign_keys.iter().enumerate() {
            let conname = fk
                .name
                .clone()
                .unwrap_or_else(|| alloc::format!("{}_fk{fi}", tname));
            for (i, (&local, &parent)) in fk
                .local_columns
                .iter()
                .zip(fk.parent_columns.iter())
                .enumerate()
            {
                let parent_name = cat
                    .get(&fk.parent_table)
                    .and_then(|pt| pt.schema().columns.get(parent).map(|c| c.name.clone()))
                    .unwrap_or_else(|| alloc::format!("col{parent}"));
                #[allow(clippy::cast_possible_wrap)]
                let ordinal = (i + 1) as i32;
                rows.push(Row::new(alloc::vec![
                    Value::text(conname.clone()),
                    Value::text(tname.clone()),
                    Value::text(col_name_at(local)),
                    Value::Int(ordinal),
                    Value::text(fk.parent_table.clone()),
                    Value::text(parent_name),
                ]));
            }
        }
        // PK / composite UC entries.
        for (ci, uc) in t.schema().uniqueness_constraints.iter().enumerate() {
            let conname = if uc.is_primary_key {
                alloc::format!("{}_pkey", tname)
            } else {
                alloc::format!("{}_uniq{ci}", tname)
            };
            for (i, &local) in uc.columns.iter().enumerate() {
                #[allow(clippy::cast_possible_wrap)]
                let ordinal = (i + 1) as i32;
                rows.push(Row::new(alloc::vec![
                    Value::text(conname.clone()),
                    Value::text(tname.clone()),
                    Value::text(col_name_at(local)),
                    Value::Int(ordinal),
                    Value::text(String::new()),
                    Value::text(String::new()),
                ]));
            }
        }
    }
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-64 — synthesise
/// `information_schema.REFERENTIAL_CONSTRAINTS`. One row per FK.
pub(crate) fn synth_info_referential_constraints(
    cat: &Catalog,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("constraint_name", DataType::Text, false),
        ColumnSchema::new("table_name", DataType::Text, false),
        ColumnSchema::new("referenced_table_name", DataType::Text, false),
        ColumnSchema::new("update_rule", DataType::Text, false),
        ColumnSchema::new("delete_rule", DataType::Text, false),
    ];
    fn rule_name(a: spg_storage::FkAction) -> &'static str {
        match a {
            spg_storage::FkAction::Cascade => "CASCADE",
            spg_storage::FkAction::SetNull => "SET NULL",
            spg_storage::FkAction::SetDefault => "SET DEFAULT",
            spg_storage::FkAction::Restrict => "RESTRICT",
            spg_storage::FkAction::NoAction => "NO ACTION",
        }
    }
    let mut rows: Vec<Row<'static>> = Vec::new();
    for tname in cat.table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        for (fi, fk) in t.schema().foreign_keys.iter().enumerate() {
            let conname = fk
                .name
                .clone()
                .unwrap_or_else(|| alloc::format!("{}_fk{fi}", tname));
            rows.push(Row::new(alloc::vec![
                Value::text(conname),
                Value::text(tname.clone()),
                Value::text(fk.parent_table.clone()),
                Value::text::<String>(rule_name(fk.on_update).into()),
                Value::text::<String>(rule_name(fk.on_delete).into()),
            ]));
        }
    }
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-64 — synthesise `information_schema.STATISTICS`.
/// One row per (index × column) — admin tools walk this to
/// surface index-cardinality estimates.
pub(crate) fn synth_info_statistics(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("table_name", DataType::Text, false),
        ColumnSchema::new("index_name", DataType::Text, false),
        ColumnSchema::new("column_name", DataType::Text, false),
        ColumnSchema::new("seq_in_index", DataType::Int, false),
        ColumnSchema::new("non_unique", DataType::Int, false),
        ColumnSchema::new("index_type", DataType::Text, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for tname in cat.table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        for idx in t.indices() {
            let col = t
                .schema()
                .columns
                .get(idx.column_position)
                .map_or("?".into(), |c| c.name.clone());
            rows.push(Row::new(alloc::vec![
                Value::text(tname.clone()),
                Value::text(idx.name.clone()),
                Value::text(col),
                Value::Int(1),
                Value::Int(i32::from(!idx.is_unique)),
                Value::text("BTREE"),
            ]));
        }
    }
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-64 — synthesise `information_schema.ROUTINES`.
/// SPG has no user-defined functions in v7.17 so the surface is
/// always empty; admin tools just need the table to exist.
pub(crate) fn synth_info_routines() -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("routine_name", DataType::Text, false),
        ColumnSchema::new("routine_type", DataType::Text, false),
        ColumnSchema::new("data_type", DataType::Text, false),
    ];
    (schema, Vec::new())
}

/// v7.17.0 Phase 3.P0-54 — synthesise `pg_catalog.pg_constraint`.
/// ORM compilers (Diesel, sea-orm) and admin tools probe this for
/// FK / UNIQUE / PK / CHECK definitions to surface relationship
/// graphs and validation rules. SPG ships one row per
/// uniqueness constraint + foreign key declared in the catalog.
///
/// Schema columns exposed:
///   * conname (Text) — constraint name (synthetic when anonymous)
///   * contype (Text) — `p` PK, `u` UNIQUE, `f` FK, `c` CHECK
///   * conrelid (Text) — owner table name
///   * confrelid (Text) — referenced parent table (FK only;
///     empty string otherwise)
///   * conkey (Text) — comma-separated column names
///   * confkey (Text) — comma-separated parent column names (FK only)
pub(crate) fn synth_pg_constraint(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("conname", DataType::Text, false),
        ColumnSchema::new("contype", DataType::Text, false),
        ColumnSchema::new("conrelid", DataType::Text, false),
        ColumnSchema::new("confrelid", DataType::Text, false),
        ColumnSchema::new("conkey", DataType::Text, false),
        ColumnSchema::new("confkey", DataType::Text, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for tname in cat.table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        let cols = &t.schema().columns;
        let col_name_at = |pos: usize| -> String {
            cols.get(pos)
                .map_or_else(|| alloc::format!("col{pos}"), |c| c.name.clone())
        };
        // Uniqueness constraints (composite UNIQUE / PRIMARY KEY).
        for (ci, uc) in t.schema().uniqueness_constraints.iter().enumerate() {
            let kind = if uc.is_primary_key { "p" } else { "u" };
            let conname = if uc.is_primary_key {
                alloc::format!("{}_pkey", tname)
            } else {
                alloc::format!("{}_uniq{ci}", tname)
            };
            let conkey: Vec<String> = uc.columns.iter().map(|&p| col_name_at(p)).collect();
            rows.push(Row::new(alloc::vec![
                Value::text(conname),
                Value::text::<String>(kind.into()),
                Value::text(tname.clone()),
                Value::text(String::new()),
                Value::text(conkey.join(",")),
                Value::text(String::new()),
            ]));
        }
        // Single-column PK / UNIQUE indexes that have no
        // matching entry in `uniqueness_constraints` (the engine
        // creates only the BTree index for the bare-column case;
        // composite forms ride the UC path above).
        for idx in t.indices() {
            if !idx.is_unique {
                continue;
            }
            let is_primary = idx.name.ends_with("_pkey");
            let conname = idx.name.clone();
            let kind = if is_primary { "p" } else { "u" };
            let col_name = col_name_at(idx.column_position);
            // Skip if already emitted via the UC loop above (same
            // tuple shape — single-column).
            let already = t
                .schema()
                .uniqueness_constraints
                .iter()
                .any(|uc| uc.columns.len() == 1 && uc.columns[0] == idx.column_position);
            if already {
                continue;
            }
            rows.push(Row::new(alloc::vec![
                Value::text(conname),
                Value::text::<String>(kind.into()),
                Value::text(tname.clone()),
                Value::text(String::new()),
                Value::text(col_name),
                Value::text(String::new()),
            ]));
        }
        // Foreign keys.
        for (fi, fk) in t.schema().foreign_keys.iter().enumerate() {
            let conname = fk
                .name
                .clone()
                .unwrap_or_else(|| alloc::format!("{}_fk{fi}", tname));
            let conkey: Vec<String> = fk.local_columns.iter().map(|&p| col_name_at(p)).collect();
            // Parent column names: look up the parent table's
            // schema if it exists; otherwise emit positions.
            let confkey: Vec<String> = if let Some(parent) = cat.get(&fk.parent_table) {
                fk.parent_columns
                    .iter()
                    .map(|&p| {
                        parent
                            .schema()
                            .columns
                            .get(p)
                            .map_or_else(|| alloc::format!("col{p}"), |c| c.name.clone())
                    })
                    .collect()
            } else {
                fk.parent_columns
                    .iter()
                    .map(|p| alloc::format!("col{p}"))
                    .collect()
            };
            rows.push(Row::new(alloc::vec![
                Value::text(conname),
                Value::text("f"),
                Value::text(tname.clone()),
                Value::text(fk.parent_table.clone()),
                Value::text(conkey.join(",")),
                Value::text(confkey.join(",")),
            ]));
        }
    }
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-55 — synthesise `pg_catalog.pg_database`.
/// SPG is single-database so we surface a single row keyed on the
/// canonical `postgres` database name (matching what every PG
/// admin tool's startup screen expects to find).
pub(crate) fn synth_pg_database(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("datname", DataType::Text, false),
        ColumnSchema::new("datdba", DataType::BigInt, false),
        ColumnSchema::new("encoding", DataType::Int, false),
        ColumnSchema::new("datcollate", DataType::Text, false),
    ];
    let rows = alloc::vec![Row::new(alloc::vec![
        Value::BigInt(16384),
        Value::text("postgres"),
        Value::BigInt(10),
        Value::Int(6), // UTF8
        Value::text("en_US.UTF-8"),
    ])];
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-55 — synthesise `pg_catalog.pg_roles`. PG's
/// pg_roles is a view over pg_authid showing all roles. SPG ships
/// one row per declared user from the engine's UserStore so admin
/// tool startup screens can populate.
pub(crate) fn synth_pg_roles(engine: &Engine) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("rolname", DataType::Text, false),
        ColumnSchema::new("rolsuper", DataType::Bool, false),
        ColumnSchema::new("rolinherit", DataType::Bool, false),
        ColumnSchema::new("rolcanlogin", DataType::Bool, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    let oid: i64 = 10;
    for (i, (name, _)) in engine.users.iter().enumerate() {
        rows.push(Row::new(alloc::vec![
            Value::BigInt(oid + (i as i64) + 1),
            Value::text(name.to_string()),
            Value::Bool(false),
            Value::Bool(true),
            Value::Bool(true),
        ]));
    }
    // Always include `postgres` as the bootstrap superuser if not
    // already present — admin tools probe for it.
    if !rows
        .iter()
        .any(|r| matches!(&r.values[1], Value::Text(s) if s == "postgres"))
    {
        rows.insert(
            0,
            Row::new(alloc::vec![
                Value::BigInt(10),
                Value::text("postgres"),
                Value::Bool(true),
                Value::Bool(true),
                Value::Bool(true),
            ]),
        );
    }
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-56 — synthesise `pg_catalog.pg_views`. PG's
/// pg_views is a view listing every catalog view; SPG ships one
/// row per declared view + its definition text.
/// Synthesise `pg_catalog.pg_extension`. SPG ships its "extension"
/// surfaces natively (vector, pg_trgm, plpgsql-shaped DO blocks), so
/// the table lists those as installed — `SELECT … FROM pg_extension
/// WHERE extname = 'vector'` probes from PG clients (mailrs embed
/// round-12) answer truthfully about capability presence.
pub(crate) fn synth_pg_extension() -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("extname", DataType::Text, false),
        ColumnSchema::new("extversion", DataType::Text, false),
        ColumnSchema::new("extnamespace", DataType::Text, false),
    ];
    let exts: &[(&str, &str)] = &[("plpgsql", "1.0"), ("vector", "0.8.0"), ("pg_trgm", "1.6")];
    let rows = exts
        .iter()
        .enumerate()
        .map(|(i, (name, ver))| {
            Row::new(alloc::vec![
                Value::BigInt(16384 + i as i64),
                Value::text::<String>((*name).into()),
                Value::text::<String>((*ver).into()),
                Value::text("pg_catalog"),
            ])
        })
        .collect();
    (schema, rows)
}

pub(crate) fn synth_pg_views(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("schemaname", DataType::Text, false),
        ColumnSchema::new("viewname", DataType::Text, false),
        ColumnSchema::new("definition", DataType::Text, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for (name, def) in cat.views() {
        rows.push(Row::new(alloc::vec![
            Value::text("public"),
            Value::text(name.clone()),
            Value::text(def.body.clone()),
        ]));
    }
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-57 — synthesise `pg_catalog.pg_settings`. ORM
/// connection-checkers (sqlx pre-flight, Diesel migrator) and admin
/// tools read `pg_settings` to discover server-side configuration.
/// SPG surfaces every session_param + a small set of canonical PG
/// defaults so the pre-flight queries match.
pub(crate) fn synth_pg_settings(engine: &Engine) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("name", DataType::Text, false),
        ColumnSchema::new("setting", DataType::Text, false),
        ColumnSchema::new("category", DataType::Text, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    // Canonical defaults every admin tool expects to find.
    let defaults: &[(&str, &str, &str)] = &[
        ("server_version", "16.0 (spg)", "Preset Options"),
        ("server_encoding", "UTF8", "Client Connection Defaults"),
        ("client_encoding", "UTF8", "Client Connection Defaults"),
        ("DateStyle", "ISO, MDY", "Client Connection Defaults"),
        ("TimeZone", "UTC", "Client Connection Defaults"),
        ("standard_conforming_strings", "on", "Compatibility"),
        ("integer_datetimes", "on", "Compatibility"),
        ("max_connections", "100", "Connections and Authentication"),
    ];
    for &(name, val, cat) in defaults {
        rows.push(Row::new(alloc::vec![
            Value::text::<String>(name.into()),
            Value::text::<String>(val.into()),
            Value::text::<String>(cat.into()),
        ]));
    }
    // Session-set params override the static defaults.
    for (k, v) in &engine.session_params {
        if !defaults
            .iter()
            .any(|(n, _, _)| (*n).eq_ignore_ascii_case(k))
        {
            rows.push(Row::new(alloc::vec![
                Value::text(k.clone()),
                Value::text(v.clone()),
                Value::text::<String>("Session".into()),
            ]));
        }
    }
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-53 — synthesise `pg_catalog.pg_indexes`.
/// PG's pg_indexes is a real view on pg_index + pg_class + pg_attribute.
/// SPG ships it as a synthesised flat table so admin tools (pgAdmin,
/// DataGrip) can list indexes by tablename without joining four catalogs.
///
/// Schema columns exposed:
///   * schemaname (Text) — always `public`
///   * tablename (Text)
///   * indexname (Text)
///   * indexdef (Text) — best-effort CREATE INDEX DDL
pub(crate) fn synth_pg_indexes(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("schemaname", DataType::Text, false),
        ColumnSchema::new("tablename", DataType::Text, false),
        ColumnSchema::new("indexname", DataType::Text, false),
        ColumnSchema::new("indexdef", DataType::Text, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for tname in cat.table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        for idx in t.indices() {
            let col_name = t
                .schema()
                .columns
                .get(idx.column_position)
                .map_or("?".into(), |c| c.name.clone());
            let unique_kw = if idx.is_unique { "UNIQUE " } else { "" };
            let indexdef = alloc::format!(
                "CREATE {unique_kw}INDEX {} ON public.{} ({})",
                idx.name,
                tname,
                col_name
            );
            rows.push(Row::new(alloc::vec![
                Value::text("public"),
                Value::text(tname.clone()),
                Value::text(idx.name.clone()),
                Value::text(indexdef),
            ]));
        }
    }
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-53 — synthesise `pg_catalog.pg_index`. The
/// "raw" pg_index catalog used by PG-internal tooling for index
/// flags and ordinal information. SPG ships the columns ORM probes
/// actually filter on.
///
/// Schema columns exposed:
///   * indexrelid (BigInt) — index OID (synthetic = position+1)
///   * indrelid (BigInt) — table OID (synthetic = position+1)
///   * indnatts (Int) — number of indexed columns
///   * indisunique (Bool)
///   * indisprimary (Bool)
pub(crate) fn synth_pg_index_raw(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("indexrelid", DataType::BigInt, false),
        ColumnSchema::new("indrelid", DataType::BigInt, false),
        ColumnSchema::new("indnatts", DataType::Int, false),
        ColumnSchema::new("indisunique", DataType::Bool, false),
        ColumnSchema::new("indisprimary", DataType::Bool, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    let mut idx_oid: i64 = 100_000;
    for (table_idx, tname) in cat.table_names().iter().enumerate() {
        let Some(t) = cat.get(tname) else { continue };
        for idx in t.indices() {
            idx_oid += 1;
            #[allow(clippy::cast_possible_wrap)]
            let nattrs = (1 + idx.extra_column_positions.len()) as i32;
            // is_primary: SPG / PG flag the primary via the
            // index name convention `<table>_pkey`.
            let is_primary = idx.name.ends_with("_pkey");
            rows.push(Row::new(alloc::vec![
                Value::BigInt(idx_oid),
                Value::BigInt((table_idx + 1) as i64),
                Value::Int(nattrs),
                Value::Bool(idx.is_unique),
                Value::Bool(is_primary),
            ]));
        }
    }
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-52 — synthesise `pg_catalog.pg_namespace`.
/// SPG is single-schema so we expose the canonical PG schemas:
/// `public` (user-facing), `pg_catalog` (built-in), and
/// `information_schema` (PG meta).
pub(crate) fn synth_pg_namespace(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("nspname", DataType::Text, false),
        ColumnSchema::new("nspowner", DataType::BigInt, false),
    ];
    let rows = alloc::vec![
        Row::new(alloc::vec![
            Value::BigInt(11),
            Value::text("pg_catalog"),
            Value::BigInt(10),
        ]),
        Row::new(alloc::vec![
            Value::BigInt(2200),
            Value::text("public"),
            Value::BigInt(10),
        ]),
        Row::new(alloc::vec![
            Value::BigInt(13000),
            Value::text("information_schema"),
            Value::BigInt(10),
        ]),
    ];
    (schema, rows)
}

/// v7.16.2 — drop the synthesised meta view into the enriched
/// catalog so the regular FROM-resolution path can see it.
pub(crate) fn materialise_meta_view(
    catalog: &mut Catalog,
    name: &str,
    columns: Vec<ColumnSchema>,
    rows: Vec<Row<'static>>,
) -> Result<(), EngineError> {
    let schema = TableSchema::new(name.to_string(), columns);
    catalog.create_table(schema).map_err(EngineError::Storage)?;
    let table = catalog
        .get_mut(name)
        .expect("just-created meta view must exist");
    for row in rows {
        table.insert(row).map_err(EngineError::Storage)?;
    }
    Ok(())
}

/// v7.16.2 — true when the SELECT statement references any
/// `__spg_info_*` or `__spg_pg_*` synthetic table name (the
/// parser produces these for `information_schema.X` /
/// `pg_catalog.X`). Used by `exec_select_cancel` to short-
/// circuit into the meta-view materialisation path.
/// v7.17.0 Phase 1.2 — append the names of any catalog-known
/// views referenced by `tref` to `into`. Helper for
/// `Engine::expand_views_in_select`. A view that's been already
/// materialised as a table (e.g. via the synthetic CTE pass for
/// SELECT FROM v) is skipped — the table form wins so the
/// recursive exec_select_cancel call inside exec_with_ctes
/// doesn't re-expand and trigger the CTE-shadow guard.
pub(crate) fn collect_view_refs(
    tref: &spg_sql::ast::TableRef,
    cat: &spg_storage::Catalog,
    into: &mut Vec<String>,
) {
    if cat.views().contains_key(&tref.name)
        && cat.get(&tref.name).is_none()
        && !into.iter().any(|n| n == &tref.name)
    {
        into.push(tref.name.clone());
    }
}

pub(crate) fn select_references_meta_view(stmt: &SelectStatement) -> bool {
    fn is_meta(name: &str) -> bool {
        name.starts_with("__spg_info_")
            || name.starts_with("__spg_pg_")
            || name.starts_with("__spg_mysql_")
    }
    if let Some(from) = &stmt.from {
        if is_meta(&from.primary.name) {
            return true;
        }
        for j in &from.joins {
            if is_meta(&j.table.name) {
                return true;
            }
        }
    }
    for cte in &stmt.ctes {
        // v7.37.43-T4.4 — only Select-bodied CTEs need meta-view
        // scanning; data-modifying CTE bodies (INSERT/UPDATE/DELETE)
        // never reference info_schema / pg_catalog views directly.
        if let Some(s) = cte.body.as_select()
            && select_references_meta_view(s)
        {
            return true;
        }
    }
    false
}

/// v7.16.2 — collect every meta-view name a SELECT touches.
/// Returns a deduplicated, sorted list. Caller materialises
/// each one into the enriched catalog before re-running the
/// SELECT. Walks JOINs, CTEs, and the primary FROM.
pub(crate) fn collect_meta_view_names(
    stmt: &SelectStatement,
    into: &mut alloc::collections::BTreeSet<String>,
) {
    fn is_meta(name: &str) -> bool {
        name.starts_with("__spg_info_")
            || name.starts_with("__spg_pg_")
            || name.starts_with("__spg_mysql_")
    }
    if let Some(from) = &stmt.from {
        if is_meta(&from.primary.name) {
            into.insert(from.primary.name.clone());
        }
        for j in &from.joins {
            if is_meta(&j.table.name) {
                into.insert(j.table.name.clone());
            }
        }
    }
    for cte in &stmt.ctes {
        if let Some(s) = cte.body.as_select() {
            collect_meta_view_names(s, into);
        }
    }
}
