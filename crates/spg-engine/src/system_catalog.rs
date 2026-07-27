//! System-catalog view synthesis — `information_schema.*`, `pg_catalog.*`,
//! and `mysql.*` metadata tables materialised on demand. Split out of
//! `lib.rs` (v7.32 engine modularisation). Each `synth_*` maps the live
//! catalog (or Engine state, for roles/settings/users) to a
//! `(schema, rows)` pair that `materialise_meta_view` installs.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use spg_sql::ast::{Expr, SelectItem, SelectStatement};
use spg_storage::{Catalog, ColumnSchema, DataType, Row, TableSchema, Value};

use crate::{Engine, EngineError};

/// PG's auto-generated name for a uniqueness constraint: `{table}_pkey`
/// for a PRIMARY KEY, else `{table}_{col…}_key` with each constrained
/// column name joined by `_`. Shared by every pg_catalog /
/// information_schema view and pg_get_constraintdef so they agree.
pub(crate) fn pg_unique_conname(
    t: &spg_storage::Table,
    uc: &spg_storage::UniquenessConstraint,
    tname: &str,
) -> String {
    // v7.39 (round 290) — a DECLARED name wins. `CONSTRAINT rdc_uq
    // UNIQUE (code)` was stored and then ignored here, so both the
    // catalog views and pg_get_constraintdef reported the synthesised
    // `rdc_code_key` — a dump would name the constraint something the
    // user never wrote.
    if let Some(n) = &uc.name {
        return n.clone();
    }
    if uc.is_primary_key {
        return alloc::format!("{tname}_pkey");
    }
    let cols = uc
        .columns
        .iter()
        .map(|&p| {
            t.schema()
                .columns
                .get(p)
                .map_or_else(|| alloc::format!("col{p}"), |c| c.name.clone())
        })
        .collect::<Vec<_>>()
        .join("_");
    alloc::format!("{tname}_{cols}_key")
}

/// PG's default foreign-key constraint name: `{table}_{col…}_fkey` over
/// the referencing (local) columns, matching pg_dump / pg_catalog. SPG
/// previously fell back to `{table}_fk{index}`, which ORMs that key off
/// the constraint name (Rails, Django introspection) don't recognise.
pub(crate) fn pg_fk_conname(
    t: &spg_storage::Table,
    fk: &spg_storage::ForeignKeyConstraint,
    tname: &str,
) -> String {
    // v7.39 (round 290) — a DECLARED name wins, the same rule the
    // uniqueness helper follows. Verified against PG: `CONSTRAINT
    // child_pid_fk FOREIGN KEY …` reports as `child_pid_fk`, not the
    // synthesised `nchild_pid_fkey`.
    if let Some(n) = &fk.name {
        return n.clone();
    }
    let cols = fk
        .local_columns
        .iter()
        .map(|&p| {
            t.schema()
                .columns
                .get(p)
                .map_or_else(|| alloc::format!("col{p}"), |c| c.name.clone())
        })
        .collect::<Vec<_>>()
        .join("_");
    alloc::format!("{tname}_{cols}_fkey")
}

/// Distinct table columns referenced by a CHECK predicate string, in
/// first-seen order. A lightweight quote-aware identifier scan (skips
/// single-quoted string literals) matched against the table's column
/// names — enough to reproduce PG's CHECK auto-naming without a full
/// re-parse.
fn referenced_columns(t: &spg_storage::Table, check: &str) -> Vec<String> {
    let bytes = check.as_bytes();
    let mut found: Vec<String> = Vec::new();
    let mut in_str = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if c == b'\'' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == b'\'' {
            in_str = true;
            i += 1;
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let ident = &check[start..i];
            if let Some(col) = t
                .schema()
                .columns
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(ident))
                && !found.iter().any(|f| f.eq_ignore_ascii_case(&col.name))
            {
                found.push(col.name.clone());
            }
        } else {
            i += 1;
        }
    }
    found
}

/// PG's auto-generated names for a table's CHECK constraints, in
/// declaration order. A check over exactly one column is
/// `{table}_{col}_check`; a multi-column / table-level check is
/// `{table}_check`. Collisions get PG's `1`, `2`, … suffix. Shared so
/// every synth view and pg_get_constraintdef agree.
pub(crate) fn pg_check_connames(
    t: &spg_storage::Table,
    tname: &str,
    checks: &[spg_storage::CheckConstraint],
) -> Vec<String> {
    let mut seen: alloc::collections::BTreeMap<String, usize> = alloc::collections::BTreeMap::new();
    let mut out = Vec::with_capacity(checks.len());
    for chk in checks {
        // v7.39 (read01 round 48) — a user-supplied name wins; only an
        // unnamed CHECK gets PG's synthesised `<table>_<col>_check` form.
        if let Some(n) = &chk.name {
            out.push(n.clone());
            continue;
        }
        let cols = referenced_columns(t, &chk.expr);
        let base = if cols.len() == 1 {
            alloc::format!("{tname}_{}_check", cols[0])
        } else {
            alloc::format!("{tname}_check")
        };
        let count = seen.entry(base.clone()).or_insert(0);
        out.push(if *count == 0 {
            base.clone()
        } else {
            alloc::format!("{base}{count}")
        });
        *count += 1;
    }
    out
}

/// v7.16.2 — map an SPG [`DataType`] to the PG-canonical
/// `information_schema.columns.data_type` text. Covers the
/// values mailrs's migrations probe (`'ARRAY'`, `'integer'`,
/// `'text'`, …). Unknown variants fall back to the SPG name
/// downcased — better than panicking on a future DataType.
/// v7.39 (round 360, M18) — MariaDB's `information_schema.data_type`
/// name (bare, no display width — that is the separate `column_type`
/// column). Measured on MariaDB 11.
pub(crate) fn mysql_data_type_text(
    ty: DataType,
    width: Option<spg_storage::MysqlIntWidth>,
) -> alloc::string::String {
    // v7.39 (round 389, epic P4a) — the bare integer name honours the
    // declared width (TINYINT / MEDIUMINT / the widened SMALLINT / INT
    // UNSIGNED), no display width and no ` unsigned` suffix (that is the
    // separate `column_type`).
    if let Some(base) = crate::show::mysql_int_base_name(ty, width) {
        return alloc::string::String::from(base);
    }
    let s = match ty {
        DataType::Float => "double",
        DataType::Real => "float",
        DataType::Numeric { .. } => "decimal",
        DataType::Bool => "tinyint",
        DataType::Text => "text",
        DataType::Varchar(_) => "varchar",
        DataType::Char(_) => "char",
        DataType::Date => "date",
        DataType::Time => "time",
        DataType::Timestamp | DataType::Timestamptz => "datetime",
        DataType::Bytes => "blob",
        DataType::Json | DataType::Jsonb => "json",
        // No MySQL spelling — fall back to the PG name, lower-cased.
        other => return pg_data_type_text(other).to_ascii_lowercase(),
    };
    alloc::string::String::from(s)
}

pub(crate) fn pg_data_type_text(ty: DataType) -> alloc::string::String {
    // Ranges report their concrete type name (`int4range`, `numrange`,
    // …); multiranges append `multirange` (`int4multirange`).
    if let DataType::Range(k) = ty {
        return alloc::string::String::from(k.keyword());
    }
    if let DataType::Multirange(k) = ty {
        let base = k.keyword();
        // keyword() yields e.g. "int4range" → "int4multirange".
        return alloc::string::String::from(base).replace("range", "multirange");
    }
    let s = match ty {
        DataType::Int => "integer",
        DataType::BigInt => "bigint",
        DataType::SmallInt => "smallint",
        DataType::Float => "double precision",
        DataType::Real => "real",
        DataType::Numeric { .. } => "numeric",
        DataType::Bool => "boolean",
        DataType::Text => "text",
        DataType::Varchar(_) => "character varying",
        DataType::Char(_) => "character",
        DataType::Char1 => "\"char\"",
        DataType::Date => "date",
        DataType::Time => "time without time zone",
        DataType::Timestamp => "timestamp without time zone",
        DataType::Timestamptz => "timestamp with time zone",
        DataType::Interval => "interval",
        DataType::Json => "jsonb",
        DataType::Jsonb => "jsonb",
        DataType::Bytes => "bytea",
        DataType::Uuid => "uuid",
        DataType::Money => "money",
        DataType::Inet => "inet",
        DataType::Cidr => "cidr",
        DataType::Macaddr => "macaddr",
        DataType::Macaddr8 => "macaddr8",
        DataType::PgLsn => "pg_lsn",
        DataType::Bit(_) => "bit",
        DataType::BitVarying(_) => "bit varying",
        DataType::Xml => "xml",
        DataType::Point => "point",
        DataType::Lseg => "lseg",
        DataType::Path => "path",
        DataType::PgBox => "box",
        DataType::Polygon => "polygon",
        DataType::Line => "line",
        DataType::Circle => "circle",
        DataType::TsVector => "tsvector",
        DataType::TsQuery => "tsquery",
        // Every array type surfaces as PG's `ARRAY` pseudo-name in
        // information_schema.columns.data_type.
        DataType::TextArray
        | DataType::IntArray
        | DataType::BigIntArray
        | DataType::SmallIntArray
        | DataType::FloatArray
        | DataType::NumericArray
        | DataType::BoolArray
        | DataType::DateArray
        | DataType::TimestampArray
        | DataType::TimestamptzArray
        | DataType::IntervalArray
        | DataType::UuidArray
        | DataType::JsonArray
        | DataType::JsonbArray
        | DataType::BytesArray
        | DataType::VarcharArray
        | DataType::CharArray
        | DataType::MoneyArray => "ARRAY",
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
    mysql: bool,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("table_catalog", DataType::Text, false),
        ColumnSchema::new("table_schema", DataType::Text, false),
        ColumnSchema::new("table_name", DataType::Text, false),
        ColumnSchema::new("column_name", DataType::Text, false),
        ColumnSchema::new("ordinal_position", DataType::Int, false),
        ColumnSchema::new("is_nullable", DataType::Text, false),
        ColumnSchema::new("data_type", DataType::Text, false),
        // v7.37.17 — widened to the columns Alembic autogenerate /
        // SQLAlchemy reflection / JDBC getColumns actually read.
        ColumnSchema::new("column_default", DataType::Text, true),
        ColumnSchema::new("character_maximum_length", DataType::Int, true),
        ColumnSchema::new("numeric_precision", DataType::Int, true),
        // v7.39 (round 269) — the radix the precision is expressed in:
        // 2 for the binary-precision types (the integers and the two
        // floats), 10 for numeric, NULL for everything non-numeric.
        // JDBC getColumns reads it alongside the precision.
        ColumnSchema::new("numeric_precision_radix", DataType::Int, true),
        ColumnSchema::new("numeric_scale", DataType::Int, true),
        ColumnSchema::new("udt_name", DataType::Text, false),
        ColumnSchema::new("is_identity", DataType::Text, false),
        // v7.39 (round 248) — the columns the sweep found missing: JDBC /
        // SQLAlchemy read identity_generation and datetime_precision, and
        // is_updatable is part of the spec's core set.
        ColumnSchema::new("identity_generation", DataType::Text, true),
        ColumnSchema::new("datetime_precision", DataType::Int, true),
        ColumnSchema::new("is_updatable", DataType::Text, false),
        // v7.39 (round 208) — generated-column columns (PG spec):
        // is_generated = ALWAYS for a generated column, else NEVER;
        // generation_expression = its source text (NULL otherwise).
        // Reflection tools (SQLAlchemy, Alembic) and pg_dump read
        // these; a query naming them previously errored (column
        // absent) — GENERATED VIRTUAL / STORED were invisible.
        ColumnSchema::new("is_generated", DataType::Text, false),
        ColumnSchema::new("generation_expression", DataType::Text, true),
    ];
    // v7.39 (round 362, M18) — MySQL's `information_schema.columns` has a
    // `column_type` column PG has no equivalent of: the full declared
    // type WITH its display width (`int(11)`, `varchar(10)`,
    // `decimal(10,2)`), which SQLAlchemy's mysql reflection reads to
    // recover a column's length and unsigned-ness. It is appended only in
    // the MySQL dialect, so the view's shape stays PG's on a PG session
    // (a PG query naming `column_type` still errors, as it does in PG).
    let mut schema = schema;
    if mysql {
        schema.push(ColumnSchema::new("column_type", DataType::Text, false));
    }
    let mut rows: Vec<Row<'static>> = Vec::new();
    for tname in cat.visible_table_names() {
        // v7.39 (round 536) — a MATERIALIZED VIEW is not in this view.
        // PG omits them from information_schema entirely (they are not in
        // the SQL standard), and `information_schema.tables` here already
        // did — so SPG listed a relation that had columns and no table
        // row, disagreeing with PG and with itself.
        if cat.materialized_views().contains_key(&tname) {
            continue;
        }
        let Some(t) = cat.get(&tname) else { continue };
        for (i, col) in t.schema().columns.iter().enumerate() {
            #[allow(clippy::cast_possible_wrap)]
            let ordinal = (i + 1) as i32;
            let mut row = info_column_row(&tname, ordinal, col, None, mysql);
            if mysql {
                row.values.push(Value::text(crate::show::render_mysql_type(col)));
            }
            rows.push(row);
        }
    }
    // v7.39 (round 268) — view columns. The view reported NO rows for a
    // view before this, so a reflection tool saw every view as a
    // relation with no columns at all. Shapes whose body does not fully
    // resolve contribute nothing rather than a guess.
    for (vname, _) in cat.views_all() {
        // v7.39 (round 469) — a temporary view belongs to one session; the
        // others must not see it listed under its mangled storage name.
        let Some(vname) = cat.listed_name(vname) else {
            continue;
        };
        let cols = crate::describe::describe_view_columns(cat, vname);
        // A column is writable only if the view itself is auto-updatable
        // AND the column is a plain base column — the same two questions
        // the write path asks (round 267).
        let updatable = crate::dml::view_is_auto_updatable(cat, vname);
        let simple = crate::dml::view_simple_column_names(cat, vname);
        for (i, col) in cols.iter().enumerate() {
            #[allow(clippy::cast_possible_wrap)]
            let ordinal = (i + 1) as i32;
            let writable = updatable && simple.iter().any(|n| n == &col.name);
            let mut row = info_column_row(vname, ordinal, col, Some(writable), mysql);
            if mysql {
                row.values.push(Value::text(crate::show::render_mysql_type(col)));
            }
            rows.push(row);
        }
    }
    (schema, rows)
}


/// v7.39 (round 268) — one `information_schema.columns` row. Tables and
/// views both come through here so the two can never describe the same
/// column shape differently; `view_updatable` is None for a table and
/// Some(writable) for a view column.
fn info_column_row(
    rel: &str,
    ordinal: i32,
    col: &ColumnSchema,
    view_updatable: Option<bool>,
    mysql: bool,
) -> Row<'static> {
        // column_default: v7.38 (read01) — the deparsed source text of the
        // DEFAULT expression (cached at CREATE TABLE), matching PG's
        // pg_get_expr output. Falls back to the serial nextval
        // spelling, else NULL.
        let default_text: Value<'static> = if let Some(txt) = &col.default_text {
            Value::text(txt.clone())
        } else if col.auto_increment {
            Value::text(alloc::format!(
                "nextval('{rel}_{}_seq'::regclass)",
                col.name
            ))
        } else {
            Value::Null
        };
        let (num_prec, num_scale): (Value<'static>, Value<'static>) = match col.ty {
            DataType::SmallInt => (Value::Int(16), Value::Int(0)),
            DataType::Int => (Value::Int(32), Value::Int(0)),
            DataType::BigInt => (Value::Int(64), Value::Int(0)),
            DataType::Float => (Value::Int(53), Value::Null),
            // v7.39 (round 269) — real carries 24 bits of mantissa.
            DataType::Real => (Value::Int(24), Value::Null),
            // v7.39 (round 272) — an UNCONSTRAINED `numeric` (the 0/0
            // sentinel) has no precision to report; PG leaves both NULL
            // there, and reporting 0 said the column holds nothing.
            DataType::Numeric { precision: 0, .. } => (Value::Null, Value::Null),
            DataType::Numeric { precision, scale } => (
                Value::Int(i32::from(precision)),
                // v7.39 (round 273) — PG reports a NEGATIVE declared scale
                // as 2048 + scale here (measured: -2 → 2046, -5 → 2043,
                // -1000 → 1048). Its typmod is stored masked and
                // information_schema reads the raw field.
                Value::Int(if scale < 0 {
                    2048 + i32::from(scale)
                } else {
                    i32::from(scale)
                }),
            ),
            _ => (Value::Null, Value::Null),
        };
        // udt_name is PG's internal typname (int4, not integer).
        let udt: &str = match col.ty {
            DataType::SmallInt => "int2",
            DataType::Int => "int4",
            DataType::BigInt => "int8",
            DataType::Float => "float8",
            DataType::Real => "float4",
            DataType::Bool => "bool",
            DataType::Text => "text",
            DataType::Bytes => "bytea",
            DataType::Json => "jsonb",
            DataType::Uuid => "uuid",
            DataType::Date => "date",
            DataType::Timestamp => "timestamp",
            // v7.38 (T-tstz Phase 1) — these fell to the `text` catch-all
            // and mis-reported themselves. PG18.4 udt_name, verified:
            // timestamptz / time / interval / numeric.
            DataType::Timestamptz => "timestamptz",
            DataType::Time => "time",
            DataType::Interval => "interval",
            DataType::Numeric { .. } => "numeric",
            // v7.39 (round 248) — varchar/char kept falling into the
            // text catch-all, and an array's udt_name is PG's
            // underscore-prefixed element name (`_text`).
            DataType::Varchar(_) => "varchar",
            DataType::Char(_) => "bpchar",
            DataType::Char1 => "char",
            DataType::TextArray => "_text",
            DataType::IntArray => "_int4",
            DataType::BigIntArray => "_int8",
            DataType::SmallIntArray => "_int2",
            DataType::FloatArray => "_float8",
            DataType::NumericArray => "_numeric",
            DataType::BoolArray => "_bool",
            DataType::DateArray => "_date",
            DataType::TimestampArray => "_timestamp",
            DataType::TimestamptzArray => "_timestamptz",
            DataType::UuidArray => "_uuid",
            _ => "text",
        };
        // v7.39 (round 248) — datetime_precision: PG reports 6 for the
        // microsecond-carrying types and 0 for date.
        let dt_prec: Value<'static> = match col.ty {
            DataType::Date => Value::Int(0),
            DataType::Time
            | DataType::Timestamp
            | DataType::Timestamptz
            | DataType::Interval => Value::Int(6),
            _ => Value::Null,
        };
    Row::new(alloc::vec![
            Value::text("spg"),
            Value::text("public"),
            Value::text(rel.to_string()),
            Value::text(col.name.clone()),
            Value::Int(ordinal),
            // A view's columns are all nullable in PG, even over a NOT
            // NULL base column: the rows are a query result and the base
            // constraint is not carried through.
            Value::text::<&str>(if view_updatable.is_some() || col.nullable {
                "YES"
            } else {
                "NO"
            }),
            // v7.39 (round 360, M18) — the `data_type` name is dialect's:
            // MariaDB reports `int` / `datetime` / `decimal` / `double`
            // where PG reports `integer` / `timestamp without time zone`
            // / `numeric` / `double precision` (both measured). A MySQL
            // reflection tool (SQLAlchemy's mysql dialect, JDBC) reads
            // this to pick the column's Python/Java type, so the PG name
            // on a MySQL session sent it down the wrong branch.
            Value::text(if mysql {
                mysql_data_type_text(col.ty, col.mysql_int_width)
            } else {
                pg_data_type_text(col.ty)
            }),
            default_text,
            // v7.39 (round 248) — a declared varchar(n)/char(n) reports
            // its limit; TEXT stays unbounded NULL.
            match col.ty {
                DataType::Varchar(n) | DataType::Char(n) => {
                    i32::try_from(n).map(Value::Int).unwrap_or(Value::Null)
                }
                _ => Value::Null,
            },
            num_prec.clone(),
            // NUMERIC states its precision in decimal digits; every
            // other numeric type states it in bits.
            match col.ty {
                DataType::Numeric { .. } => Value::Int(10),
                _ if matches!(num_prec, Value::Null) => Value::Null,
                _ => Value::Int(2),
            },
            num_scale,
            Value::text::<&str>(udt),
            // v7.39 (round 248) — a SERIAL column is NOT identity in PG
            // (is_identity keys off GENERATED … AS IDENTITY). SPG only
            // records the ALWAYS flavour, so BY DEFAULT identity reports
            // NO here — recorded residual (needs a catalog field).
            Value::text::<&str>(if col.identity_always { "YES" } else { "NO" }),
            if col.identity_always {
                Value::text::<&str>("ALWAYS")
            } else {
                Value::Null
            },
            dt_prec,
            Value::text::<&str>(match view_updatable {
                Some(false) => "NO",
                _ => "YES",
            }),
            Value::text::<&str>(if col.generated_stored_expr.is_some() {
                "ALWAYS"
            } else {
                "NEVER"
            }),
            match &col.generated_stored_expr {
                Some(src) => Value::text(src.clone()),
                None => Value::Null,
            },
    ])
}

/// v7.39 (round 277) — synthesise `pg_catalog.pg_prepared_statements`.
/// One row per SQL-level prepared statement in THIS session. PG reports
/// the whole `PREPARE …` text as `statement` and the declared parameter
/// types as a text array.
pub(crate) fn synth_pg_prepared_statements(
    prepared: &alloc::collections::BTreeMap<String, crate::PreparedSqlStatement>,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("name", DataType::Text, false),
        ColumnSchema::new("statement", DataType::Text, false),
        ColumnSchema::new("parameter_types", DataType::TextArray, false),
        ColumnSchema::new("from_sql", DataType::Bool, false),
    ];
    let rows: Vec<Row<'static>> = prepared
        .iter()
        .map(|(name, p)| {
            Row::new(alloc::vec![
                Value::text(name.clone()),
                Value::text(p.source.clone()),
                Value::TextArray(
                    p.param_types
                        .iter()
                        // PG reports the CANONICAL type name, so a
                        // declared `int` comes back `integer`. An
                        // unrecognised name passes through as written.
                        .map(|t| {
                            Some(
                                crate::conversions::type_name_to_data_type(t)
                                    .map_or_else(|| t.clone(), pg_data_type_text),
                            )
                        })
                        .collect(),
                ),
                // Every entry here arrived through SQL PREPARE; the
                // extended-query path keeps its named plans elsewhere.
                Value::Bool(true),
            ])
        })
        .collect();
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
        // v7.39 (round 266) — the SQL-standard tail of the view. PG
        // leaves every one of these NULL for an ordinary table and
        // answers YES/NO for the two flags, so the columns exist to be
        // *read*: reflection tools select them by name and previously
        // got "column does not exist" instead of PG's NULL.
        ColumnSchema::new("self_referencing_column_name", DataType::Text, true),
        ColumnSchema::new("reference_generation", DataType::Text, true),
        ColumnSchema::new("user_defined_type_catalog", DataType::Text, true),
        ColumnSchema::new("user_defined_type_schema", DataType::Text, true),
        ColumnSchema::new("user_defined_type_name", DataType::Text, true),
        ColumnSchema::new("is_insertable_into", DataType::Text, false),
        ColumnSchema::new("is_typed", DataType::Text, false),
        ColumnSchema::new("commit_action", DataType::Text, true),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for tname in cat.visible_table_names() {
        // v7.39 (round 267) — a materialized view is backed by a real
        // table in SPG, but PG omits materialized views from this view
        // entirely (they are not in the SQL standard), and reporting one
        // as a BASE TABLE would have a migration tool try to recreate it
        // as a table.
        if cat.materialized_views().contains_key(&tname) {
            continue;
        }
        rows.push(Row::new(alloc::vec![
            Value::text("spg"),
            Value::text("public"),
            Value::text(tname.clone()),
            Value::text("BASE TABLE"),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::text("YES"),
            Value::text("NO"),
            Value::Null,
        ]));
    }
    // v7.39 (round 267) — views. The view was table-only, so every
    // reflection tool saw a database with no views in it. table_type is
    // VIEW and is_insertable_into follows the same auto-updatability
    // judgement the write path uses.
    for (name, _) in cat.views_all() {
        let Some(name) = cat.listed_name(name) else {
            continue;
        };
        let insertable = crate::dml::view_is_auto_updatable(cat, name);
        rows.push(Row::new(alloc::vec![
            Value::text("spg"),
            Value::text("public"),
            Value::text(name.to_string()),
            Value::text("VIEW"),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::text::<&str>(if insertable { "YES" } else { "NO" }),
            Value::text("NO"),
            Value::Null,
        ]));
    }
    (schema, rows)
}

/// v7.37.24 (24.9) — synthesise `information_schema.schemata`.
/// SQL-standard view listing every schema in the catalog. SPG
/// is single-schema (`public`); pg_catalog + information_schema
/// also list as standard PG namespaces. dump/migration tools
/// (Liquibase, Flyway) query this at start-up to validate the
/// target connection.
pub(crate) fn synth_information_schema_schemata(
    _cat: &Catalog,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("catalog_name", DataType::Text, false),
        ColumnSchema::new("schema_name", DataType::Text, false),
        ColumnSchema::new("schema_owner", DataType::Text, false),
        ColumnSchema::new("default_character_set_catalog", DataType::Text, true),
        ColumnSchema::new("default_character_set_schema", DataType::Text, true),
        ColumnSchema::new("default_character_set_name", DataType::Text, true),
        ColumnSchema::new("sql_path", DataType::Text, true),
    ];
    let rows: Vec<Row<'static>> = alloc::vec![
        Row::new(alloc::vec![
            Value::text("spg"),
            Value::text("public"),
            Value::text("admin"),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ]),
        Row::new(alloc::vec![
            Value::text("spg"),
            Value::text("pg_catalog"),
            Value::text("admin"),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ]),
        Row::new(alloc::vec![
            Value::text("spg"),
            Value::text("information_schema"),
            Value::text("admin"),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ]),
    ];
    (schema, rows)
}

/// v7.37.24 (24.9) — synthesise `information_schema.views`.
/// PG-standard surface listing every view. SPG's view storage
/// keeps the view definition on the catalog; this surfaces the
/// `view_definition` (SQL text) per row, which is what pgAdmin
/// and ORM introspection tools consume.
pub(crate) fn synth_information_schema_views(
    cat: &Catalog,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("table_catalog", DataType::Text, false),
        ColumnSchema::new("table_schema", DataType::Text, false),
        ColumnSchema::new("table_name", DataType::Text, false),
        ColumnSchema::new("view_definition", DataType::Text, true),
        ColumnSchema::new("check_option", DataType::Text, false),
        ColumnSchema::new("is_updatable", DataType::Text, false),
        ColumnSchema::new("is_insertable_into", DataType::Text, false),
        ColumnSchema::new("is_trigger_updatable", DataType::Text, false),
        ColumnSchema::new("is_trigger_deletable", DataType::Text, false),
        ColumnSchema::new("is_trigger_insertable_into", DataType::Text, false),
    ];
    let rows: Vec<Row<'static>> = cat
        .views_all()
        .values()
        // v7.39 (round 469) — a temporary view belongs to one session; the
        // others must not see it listed under its mangled storage name.
        .filter_map(|v| cat.listed_name(&v.name).map(|n| (n.to_string(), v)))
        .map(|(vname, v)| {
            Row::new(alloc::vec![
                Value::text("spg"),
                Value::text("public"),
                Value::text(vname),
                // v7.39 (round 336, V58) — the DEPARSED definition, the same
                // text `pg_get_viewdef` gives. This used to be the stored
                // body verbatim, which meant no layout and — worse — SPG's
                // internal `count_star()` spelling reaching a client that
                // introspects views (PG says `count(*)`).
                Value::text(crate::eval::functions::pg_viewdef_render(&v.body, false)),
                // v7.39 (round 132) — WITH CHECK OPTION: 0=NONE, 1=LOCAL, 2=CASCADED.
                Value::text(match v.check_option {
                    1 => "LOCAL",
                    2 => "CASCADED",
                    _ => "NONE",
                }),
                // v7.39 (round 267) — the same auto-updatability
                // judgement the write path uses. These read NO for every
                // view until this round, while INSERT/UPDATE/DELETE
                // through a simple view had worked since 19.13: the
                // catalog was telling every reflection tool the engine
                // could not do something it does.
                Value::text::<&str>(if crate::dml::view_is_auto_updatable(cat, &v.name) {
                    "YES"
                } else {
                    "NO"
                }),
                Value::text::<&str>(if crate::dml::view_is_auto_updatable(cat, &v.name) {
                    "YES"
                } else {
                    "NO"
                }),
                // SPG has no INSTEAD OF-trigger-driven updatability to
                // report separately; PG answers NO for a plain view too.
                Value::text("NO"),
                Value::text("NO"),
                Value::text("NO"),
            ])
        })
        .collect();
    (schema, rows)
}

/// v7.37.22 (22.20) — synthesise `pg_catalog.pg_stat_progress_vacuum`.
/// PG's per-VACUUM-in-progress view that monitoring dashboards
/// poll while a long-running vacuum is active. SPG's vacuum
/// daemon lands with v7.37.15 (Phase D); the view ships
/// shape-stable empty so monitoring queries don't break.
///
/// PG-canonical columns:
///   * pid (Int)
///   * datid (BigInt) — database OID
///   * datname (Text)
///   * relid (BigInt) — table being vacuumed
///   * phase (Text) — 'initializing' / 'scanning heap' / etc.
///   * heap_blks_total / heap_blks_scanned / heap_blks_vacuumed (BigInt)
///   * index_vacuum_count (BigInt)
///   * max_dead_tuples / num_dead_tuples (BigInt)
pub(crate) fn synth_pg_stat_progress_vacuum(
    _cat: &Catalog,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("pid", DataType::Int, false),
        ColumnSchema::new("datid", DataType::BigInt, false),
        ColumnSchema::new("datname", DataType::Text, false),
        ColumnSchema::new("relid", DataType::BigInt, false),
        ColumnSchema::new("phase", DataType::Text, false),
        ColumnSchema::new("heap_blks_total", DataType::BigInt, false),
        ColumnSchema::new("heap_blks_scanned", DataType::BigInt, false),
        ColumnSchema::new("heap_blks_vacuumed", DataType::BigInt, false),
        ColumnSchema::new("index_vacuum_count", DataType::BigInt, false),
        ColumnSchema::new("max_dead_tuples", DataType::BigInt, false),
        ColumnSchema::new("num_dead_tuples", DataType::BigInt, false),
    ];
    let rows: Vec<Row<'static>> = Vec::new();
    (schema, rows)
}

/// v7.37.22 (22.21) — synthesise `pg_catalog.pg_stat_progress_create_index`.
/// PG's per-CREATE-INDEX-in-progress view. SPG's CREATE INDEX
/// is synchronous and finishes inside the wire path; the view
/// shape-stable empties so monitoring queries match PG's
/// "no active build" case.
pub(crate) fn synth_pg_stat_progress_create_index(
    _cat: &Catalog,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("pid", DataType::Int, false),
        ColumnSchema::new("datid", DataType::BigInt, false),
        ColumnSchema::new("datname", DataType::Text, false),
        ColumnSchema::new("relid", DataType::BigInt, false),
        ColumnSchema::new("index_relid", DataType::BigInt, false),
        ColumnSchema::new("command", DataType::Text, false),
        ColumnSchema::new("phase", DataType::Text, false),
        ColumnSchema::new("lockers_total", DataType::BigInt, false),
        ColumnSchema::new("lockers_done", DataType::BigInt, false),
        ColumnSchema::new("current_locker_pid", DataType::Int, false),
        ColumnSchema::new("blocks_total", DataType::BigInt, false),
        ColumnSchema::new("blocks_done", DataType::BigInt, false),
        ColumnSchema::new("tuples_total", DataType::BigInt, false),
        ColumnSchema::new("tuples_done", DataType::BigInt, false),
        ColumnSchema::new("partitions_total", DataType::BigInt, false),
        ColumnSchema::new("partitions_done", DataType::BigInt, false),
    ];
    let rows: Vec<Row<'static>> = Vec::new();
    (schema, rows)
}

/// v7.37.22 (22.22) — synthesise
/// `pg_catalog.pg_stat_progress_analyze`. Empty until v7.37.22
/// (22.3) autoanalyze_pass wires per-table progress reporting.
pub(crate) fn synth_pg_stat_progress_analyze(
    _cat: &Catalog,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("pid", DataType::Int, false),
        ColumnSchema::new("datid", DataType::BigInt, false),
        ColumnSchema::new("datname", DataType::Text, false),
        ColumnSchema::new("relid", DataType::BigInt, false),
        ColumnSchema::new("phase", DataType::Text, false),
        ColumnSchema::new("sample_blks_total", DataType::BigInt, false),
        ColumnSchema::new("sample_blks_scanned", DataType::BigInt, false),
        ColumnSchema::new("ext_stats_total", DataType::BigInt, false),
        ColumnSchema::new("ext_stats_computed", DataType::BigInt, false),
        ColumnSchema::new("child_tables_total", DataType::BigInt, false),
        ColumnSchema::new("child_tables_done", DataType::BigInt, false),
        ColumnSchema::new("current_child_table_relid", DataType::BigInt, false),
    ];
    let rows: Vec<Row<'static>> = Vec::new();
    (schema, rows)
}

/// v7.37.24 (24.16) — synthesise `pg_catalog.pg_inherits`.
/// PG's catalog table that walks the parent → child inheritance
/// graph. pg_dump uses this to restore CREATE TABLE …
/// PARTITION OF parent declarations; partition-aware monitoring
/// dashboards walk it to map partition children back to parents.
///
/// PG-canonical columns:
///   * inhrelid (BigInt) — child OID
///   * inhparent (BigInt) — parent OID
///   * inhseqno (Int) — 1-based order within parent
///   * inhdetachpending (Bool) — false in SPG (DETACH is atomic)
///
/// SPG declarative partitioning (v7.37.6-B + v7.37.16) is the
/// only inheritance source; legacy CREATE TABLE … INHERITS
/// (v7.37.18 (18.9) accept-and-no-op) doesn't materialise here.
pub(crate) fn synth_pg_inherits(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    use spg_storage::PartitionRole;
    let schema = alloc::vec![
        ColumnSchema::new("inhrelid", DataType::BigInt, false),
        ColumnSchema::new("inhparent", DataType::BigInt, false),
        ColumnSchema::new("inhseqno", DataType::Int, false),
        ColumnSchema::new("inhdetachpending", DataType::Bool, false),
    ];
    // Build name→OID map matching pg_class's 16384+ band.
    let mut by_name: alloc::collections::BTreeMap<String, i64> =
        alloc::collections::BTreeMap::new();
    let mut oid: i64 = 16384;
    for tname in cat.visible_table_names() {
        by_name.insert(tname.clone(), oid);
        oid = oid.saturating_add(1);
    }
    let mut rows: Vec<Row<'static>> = Vec::new();
    // Track per-parent seqno so each child gets a unique 1-based
    // index — matches PG's pg_inherits.inhseqno semantics.
    let mut per_parent_seq: alloc::collections::BTreeMap<i64, i32> =
        alloc::collections::BTreeMap::new();
    for cname in cat.visible_table_names() {
        let Some(c) = cat.get(&cname) else { continue };
        let parent_name = match &c.schema().partition_role {
            Some(PartitionRole::Range { parent_name, .. })
            | Some(PartitionRole::List { parent_name, .. })
            | Some(PartitionRole::Hash { parent_name, .. })
            | Some(PartitionRole::Default { parent_name }) => parent_name.clone(),
            _ => continue,
        };
        let Some(&child_oid) = by_name.get(&cname) else {
            continue;
        };
        let Some(&parent_oid) = by_name.get(&parent_name) else {
            continue;
        };
        let seq = per_parent_seq
            .entry(parent_oid)
            .and_modify(|n| *n += 1)
            .or_insert(1);
        rows.push(Row::new(alloc::vec![
            Value::BigInt(child_oid),
            Value::BigInt(parent_oid),
            Value::Int(*seq),
            Value::Bool(false),
        ]));
    }
    (schema, rows)
}

/// v7.37.24 (24.17) — synthesise `pg_catalog.pg_depend`. PG's
/// dependency-graph table that pg_dump walks to figure out
/// drop order (drop dependent objects before their parents).
/// SPG doesn't track per-object dependencies explicitly (the
/// DROP-time enforcement is per-kind: FK → check parent table
/// exists; INDEX → table exists; etc.), so the view ships
/// empty with the PG-canonical column shape. pg_dump's
/// dependency-walking query returns no rows → drop in declared
/// order → still correct for SPG's hard-coded enforcement
/// graph.
///
/// PG-canonical columns:
///   * classid (BigInt) — pg_class OID for the dependent's catalog
///   * objid (BigInt) — dependent OID
///   * objsubid (Int) — column position for column-level deps
///   * refclassid (BigInt) — pg_class OID for the referenced's catalog
///   * refobjid (BigInt) — referenced OID
///   * refobjsubid (Int)
///   * deptype (Text) — single char: 'n' normal / 'a' auto /
///     'i' internal / 'e' extension / 'p' pin
pub(crate) fn synth_pg_depend(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("classid", DataType::BigInt, false),
        ColumnSchema::new("objid", DataType::BigInt, false),
        ColumnSchema::new("objsubid", DataType::Int, false),
        ColumnSchema::new("refclassid", DataType::BigInt, false),
        ColumnSchema::new("refobjid", DataType::BigInt, false),
        ColumnSchema::new("refobjsubid", DataType::Int, false),
        ColumnSchema::new("deptype", DataType::Text, false),
    ];
    let rows: Vec<Row<'static>> = Vec::new();
    (schema, rows)
}

/// v7.38 (read01) — synthesise `pg_catalog.pg_attrdef`, the column-default
/// catalog. One row per column that has an explicit DEFAULT, carrying the
/// deparsed source text in `adbin` (SPG stores the PG-compatible text there
/// rather than a real `pg_node_tree`; `pg_get_expr(adbin, adrelid)` returns it
/// verbatim). ORM reflection (SQLAlchemy, Alembic autogenerate) and pg_dump
/// read this via `SELECT adnum, pg_get_expr(adbin, adrelid) FROM pg_attrdef`.
///
/// PG-canonical columns:
///   * oid (BigInt) — the attrdef row OID (synthetic; not asserted-parity)
///   * adrelid (BigInt) — owning table OID (pg_class's 16384+ band)
///   * adnum (SmallInt) — 1-based column position
///   * adbin (Text) — the default expression (pg_node_tree in PG)
pub(crate) fn synth_pg_attrdef(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("adrelid", DataType::BigInt, false),
        ColumnSchema::new("adnum", DataType::SmallInt, false),
        ColumnSchema::new("adbin", DataType::Text, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    let mut table_oid: i64 = 16384;
    for tname in cat.visible_table_names() {
        let Some(t) = cat.get(&tname) else {
            table_oid = table_oid.saturating_add(1);
            continue;
        };
        for (i, col) in t.schema().columns.iter().enumerate() {
            let Some(txt) = &col.default_text else {
                continue;
            };
            #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
            let adnum = (i + 1) as i16;
            // Synthetic row OID: table OID × 1000 + column position. Distinct
            // per default, in a band that won't collide with table OIDs.
            let row_oid = table_oid
                .saturating_mul(1000)
                .saturating_add(i64::from(adnum));
            rows.push(Row::new(alloc::vec![
                Value::BigInt(row_oid),
                Value::BigInt(table_oid),
                Value::SmallInt(adnum),
                Value::text(txt.clone()),
            ]));
        }
        table_oid = table_oid.saturating_add(1);
    }
    (schema, rows)
}

/// v7.39 (RLS) — synthesise `pg_catalog.pg_policy` (raw). One row per policy.
/// `polqual` / `polwithcheck` hold the deparsed qual text (SPG has no real
/// node tree); `pg_get_expr(polqual, polrelid)` returns it verbatim, matching
/// the pg_attrdef.adbin convention.
///
/// PG columns: oid, polname, polrelid, polcmd (char), polpermissive (bool),
/// polroles (oid[]), polqual (pg_node_tree), polwithcheck (pg_node_tree).
pub(crate) fn synth_pg_policy(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("polname", DataType::Text, false),
        ColumnSchema::new("polrelid", DataType::BigInt, false),
        ColumnSchema::new("polcmd", DataType::Text, false),
        ColumnSchema::new("polpermissive", DataType::Bool, false),
        ColumnSchema::new("polroles", DataType::Text, false),
        ColumnSchema::new("polqual", DataType::Text, true),
        ColumnSchema::new("polwithcheck", DataType::Text, true),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    let mut table_oid: i64 = 16384;
    for tname in cat.visible_table_names() {
        let Some(t) = cat.get(&tname) else {
            table_oid = table_oid.saturating_add(1);
            continue;
        };
        for (i, p) in t.schema().policies.iter().enumerate() {
            #[allow(clippy::cast_possible_wrap)]
            let row_oid = table_oid.saturating_mul(1000).saturating_add(i as i64 + 1);
            let roles = if p.roles.is_empty() {
                // PUBLIC — PG's polroles = {0}.
                alloc::string::String::from("{0}")
            } else {
                alloc::format!("{{{}}}", p.roles.join(","))
            };
            rows.push(Row::new(alloc::vec![
                Value::BigInt(row_oid),
                Value::text(p.name.clone()),
                Value::BigInt(table_oid),
                Value::text(alloc::string::String::from(p.cmd.as_pg_char())),
                Value::Bool(p.permissive),
                Value::text(roles),
                p.using_expr.clone().map_or(Value::Null, Value::text),
                p.with_check_expr.clone().map_or(Value::Null, Value::text),
            ]));
        }
        table_oid = table_oid.saturating_add(1);
    }
    (schema, rows)
}

/// v7.39 (RLS) — synthesise `pg_catalog.pg_policies` (the human-readable view
/// over pg_policy). ORM / psql `\d` read this.
///
/// PG columns: schemaname, tablename, policyname, permissive
/// ('PERMISSIVE'|'RESTRICTIVE'), roles (name[]), cmd (word), qual, with_check.
pub(crate) fn synth_pg_policies(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("schemaname", DataType::Text, false),
        ColumnSchema::new("tablename", DataType::Text, false),
        ColumnSchema::new("policyname", DataType::Text, false),
        ColumnSchema::new("permissive", DataType::Text, false),
        ColumnSchema::new("roles", DataType::Text, false),
        ColumnSchema::new("cmd", DataType::Text, false),
        ColumnSchema::new("qual", DataType::Text, true),
        ColumnSchema::new("with_check", DataType::Text, true),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for tname in cat.visible_table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        for p in &t.schema().policies {
            let roles = if p.roles.is_empty() {
                alloc::string::String::from("{public}")
            } else {
                alloc::format!("{{{}}}", p.roles.join(","))
            };
            rows.push(Row::new(alloc::vec![
                Value::text("public"),
                Value::text(tname.clone()),
                Value::text(p.name.clone()),
                Value::text(if p.permissive {
                    "PERMISSIVE"
                } else {
                    "RESTRICTIVE"
                }),
                Value::text(roles),
                Value::text(p.cmd.as_pg_word()),
                p.using_expr.clone().map_or(Value::Null, Value::text),
                p.with_check_expr.clone().map_or(Value::Null, Value::text),
            ]));
        }
    }
    (schema, rows)
}

/// v7.39 (round 287) — synthesise `pg_catalog.pg_largeobject`.
///
/// PG stores a large object as 2 KB pages, one row per page, and that
/// page size is observable: a 5000-byte object is three rows of
/// 2048 / 2048 / 904. SPG keeps the whole byte string and slices it
/// here, so the storage form stays SPG's while the view matches.
pub(crate) fn synth_pg_largeobject(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    /// PG's LOBLKSIZE (BLCKSZ/4 at the default 8 KB block).
    const PAGE: usize = 2048;
    let schema = alloc::vec![
        ColumnSchema::new("loid", DataType::BigInt, false),
        ColumnSchema::new("pageno", DataType::Int, false),
        ColumnSchema::new("data", DataType::Bytes, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for (oid, bytes) in cat.large_objects() {
        // An empty object has no page rows at all — PG writes none.
        for (pageno, chunk) in bytes.chunks(PAGE).enumerate() {
            rows.push(Row::new(alloc::vec![
                Value::BigInt(i64::from(*oid)),
                Value::Int(i32::try_from(pageno).unwrap_or(i32::MAX)),
                Value::Bytes(chunk.to_vec().into()),
            ]));
        }
    }
    (schema, rows)
}

/// v7.39 (round 287) — synthesise `pg_catalog.pg_largeobject_metadata`.
/// One row per object, whether or not it has any page rows — which is
/// how an empty large object is still discoverable.
pub(crate) fn synth_pg_largeobject_metadata(
    cat: &Catalog,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("lomowner", DataType::BigInt, false),
        ColumnSchema::new("lomacl", DataType::Text, true),
    ];
    let rows: Vec<Row<'static>> = cat
        .large_objects()
        .keys()
        .map(|oid| {
            Row::new(alloc::vec![
                Value::BigInt(i64::from(*oid)),
                Value::BigInt(10),
                Value::Null,
            ])
        })
        .collect();
    (schema, rows)
}

/// v7.37.23 (23.7-a) — synthesise `pg_catalog.pg_statistic_ext`.
/// PG's extended-statistics catalog (one row per CREATE
/// STATISTICS). SPG accepts CREATE STATISTICS as a parser
/// no-op today; the view ships empty so the column shape is
/// stable. When the engine wires real extended-stats tracking
/// (v7.38 candidate), rows light up here.
///
/// PG-canonical columns (subset that pg_dump + monitoring
/// queries read at handshake):
///   * oid (BigInt)
///   * stxrelid (BigInt) — owning table OID
///   * stxname (Text) — statistics name
///   * stxnamespace (BigInt)
///   * stxowner (BigInt)
///   * stxkind (Text) — "{d,f,m,e}" array flattened to comma
///   * stxkeys (Text) — int2vector of column positions, flat
pub(crate) fn synth_pg_statistic_ext(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("stxrelid", DataType::BigInt, false),
        ColumnSchema::new("stxname", DataType::Text, false),
        ColumnSchema::new("stxnamespace", DataType::BigInt, false),
        ColumnSchema::new("stxowner", DataType::BigInt, false),
        ColumnSchema::new("stxkind", DataType::Text, false),
        ColumnSchema::new("stxkeys", DataType::Text, false),
    ];
    // v7.39 (round 280) — one row per catalogued CREATE STATISTICS.
    // The view was shape-stable empty because the statement was
    // swallowed; now it reports what the catalog holds.
    let rows: Vec<Row<'static>> = cat
        .statistics_ext()
        .iter()
        .map(|st| {
            Row::new(alloc::vec![
                Value::BigInt(0),
                Value::BigInt(0),
                Value::text(st.name.clone()),
                Value::BigInt(2200),
                Value::BigInt(0),
                Value::text(alloc::format!("{{{}}}", st.kinds.join(","))),
                Value::text(st.columns.join(" ")),
            ])
        })
        .collect();
    (schema, rows)
}

/// v7.37.24 (24.15) — synthesise `pg_catalog.pg_statistic`.
/// PG's per-column statistics — the auto-collected histograms /
/// MCV lists ANALYZE writes. SPG keeps live histograms in the
/// `Statistics` engine module (see crates/spg-engine/src/
/// statistics.rs); the view materialises one row per
/// (table, column) pair so PG planner dashboards have a
/// fan-out to query against. The actual histogram bytes
/// (stavalues1 / stanumbers1 …) are deferred to v7.38 — the
/// shape lands now so the dashboards parse.
///
/// PG-canonical columns (subset):
///   * starelid (BigInt) — table OID
///   * staattnum (SmallInt) — column position
///   * stainherit (Bool) — inheritance flag (false in SPG)
///   * stanullfrac (Float) — fraction of NULLs
///   * stawidth (Int) — avg byte width
///   * stadistinct (Float) — distinct estimate
pub(crate) fn synth_pg_statistic(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("starelid", DataType::BigInt, false),
        ColumnSchema::new("staattnum", DataType::SmallInt, false),
        ColumnSchema::new("stainherit", DataType::Bool, false),
        ColumnSchema::new("stanullfrac", DataType::Float, false),
        ColumnSchema::new("stawidth", DataType::Int, false),
        ColumnSchema::new("stadistinct", DataType::Float, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    let mut starelid: i64 = 16384;
    for name in cat.visible_table_names() {
        if crate::is_internal_table_name(&name) {
            continue;
        }
        let Some(t) = cat.get(&name) else {
            continue;
        };
        #[allow(clippy::cast_possible_wrap)]
        for (i, _col) in t.schema().columns.iter().enumerate() {
            let attnum = (i + 1) as i16;
            rows.push(Row::new(alloc::vec![
                Value::BigInt(starelid),
                Value::SmallInt(attnum),
                Value::Bool(false),
                Value::Float(0.0),
                Value::Int(0),
                Value::Float(0.0),
            ]));
        }
        starelid = starelid.saturating_add(1);
    }
    (schema, rows)
}

/// v7.37.22 (22.18) — synthesise `pg_catalog.pg_stat_io` (PG 16+).
/// One row per (backend_type, object, context) combo. Modern
/// pgwatch / pganalyze dashboards prefer this surface over the
/// older pg_statio_* views. SPG ships the column shape with a
/// single aggregate row so the view doesn't return empty; per-
/// backend wiring lands when v7.37.15 MVCC + spg-server's
/// per-connection accounting are both in place.
///
/// PG-canonical columns:
///   * backend_type (Text) — 'client backend' / 'background writer' / etc.
///   * object (Text) — 'relation' / 'temp relation'
///   * context (Text) — 'normal' / 'vacuum' / 'bulkread' / 'bulkwrite'
///   * reads / read_time / writes / write_time / writebacks /
///     writeback_time / extends / extend_time / op_bytes /
///     hits / evictions / reuses / fsyncs / fsync_time (BigInt/Float)
///   * stats_reset (TIMESTAMPTZ)
pub(crate) fn synth_pg_stat_io(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("backend_type", DataType::Text, false),
        ColumnSchema::new("object", DataType::Text, false),
        ColumnSchema::new("context", DataType::Text, false),
        ColumnSchema::new("reads", DataType::BigInt, false),
        ColumnSchema::new("read_time", DataType::Float, false),
        ColumnSchema::new("writes", DataType::BigInt, false),
        ColumnSchema::new("write_time", DataType::Float, false),
        ColumnSchema::new("writebacks", DataType::BigInt, false),
        ColumnSchema::new("writeback_time", DataType::Float, false),
        ColumnSchema::new("extends", DataType::BigInt, false),
        ColumnSchema::new("extend_time", DataType::Float, false),
        ColumnSchema::new("op_bytes", DataType::BigInt, false),
        ColumnSchema::new("hits", DataType::BigInt, false),
        ColumnSchema::new("evictions", DataType::BigInt, false),
        ColumnSchema::new("reuses", DataType::BigInt, false),
        ColumnSchema::new("fsyncs", DataType::BigInt, false),
        ColumnSchema::new("fsync_time", DataType::Float, false),
        ColumnSchema::new("stats_reset", DataType::Timestamptz, true),
    ];
    // Single aggregate row to keep the SELECT non-empty;
    // dashboards' SUM(reads) / AVG(read_time) queries return 0
    // rather than NULL.
    let rows = alloc::vec![Row::new(alloc::vec![
        Value::text("client backend"),
        Value::text("relation"),
        Value::text("normal"),
        Value::BigInt(0),
        Value::Float(0.0),
        Value::BigInt(0),
        Value::Float(0.0),
        Value::BigInt(0),
        Value::Float(0.0),
        Value::BigInt(0),
        Value::Float(0.0),
        Value::BigInt(8192), // PG-canonical default
        Value::BigInt(0),
        Value::BigInt(0),
        Value::BigInt(0),
        Value::BigInt(0),
        Value::Float(0.0),
        Value::Null,
    ])];
    (schema, rows)
}

/// v7.37.22 (22.19) — synthesise `pg_catalog.pg_stat_user_functions`.
/// PG's per-function call-count + timing scrape view. SPG hasn't
/// surfaced per-function call counters yet; the view ships
/// empty so monitoring `SELECT funcname FROM
/// pg_stat_user_functions WHERE calls > 100` queries return
/// no rows (vs returning a parse error). Per-function wiring
/// lands when PL/pgSQL (v7.37.20) ships and the call-site
/// counter exists.
pub(crate) fn synth_pg_stat_user_functions(
    _cat: &Catalog,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("funcid", DataType::BigInt, false),
        ColumnSchema::new("schemaname", DataType::Text, false),
        ColumnSchema::new("funcname", DataType::Text, false),
        ColumnSchema::new("calls", DataType::BigInt, false),
        ColumnSchema::new("total_time", DataType::Float, false),
        ColumnSchema::new("self_time", DataType::Float, false),
    ];
    let rows: Vec<Row<'static>> = Vec::new();
    (schema, rows)
}

/// v7.39 (read01 round 53) — the pg_am oid backing an index, derived from what
/// the index ACTUALLY is. `USING hash` / `USING gist` are accepted but built as
/// btree, so they report btree — the catalog never claims an AM SPG lacks.
pub(crate) const fn am_oid_of(kind: &spg_storage::IndexKind) -> i64 {
    use spg_storage::IndexKind as K;
    match kind {
        K::Nsw(_) => 0, // hnsw is an extension AM; no core oid
        K::Brin { .. } => 3580,
        K::Gin(_) | K::GinTrgm(_) | K::GinFulltext(_) | K::GinJsonb(_) => 2742,
        _ => 403, // btree
    }
}

/// v7.37.24 (24.13) — synthesise `pg_catalog.pg_am`.
/// PG index/table access methods (heap, btree, hash, gist,
/// gin, spgist, brin). pg_dump queries this to validate the AM
/// for each index it emits; ORMs that follow `pg_class.relam`
/// FK to learn how to query an index also read it. SPG ships
/// 2 AMs today: `heap` (the table AM) and `btree` (the only
/// real index AM; nsw / bloom / brin live as engine-private
/// index kinds — they surface as `btree` via pg_class to keep
/// the join shape stable).
pub(crate) fn synth_pg_am(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("amname", DataType::Text, false),
        ColumnSchema::new("amhandler", DataType::BigInt, false),
        ColumnSchema::new("amtype", DataType::Text, false), // 't' table / 'i' index
    ];
    // v7.39 (read01 round 52/53) — every AM PG ships, at PG's own oids, so a
    // join through pg_class.relam lands on the right name. SPG implements
    // btree / gin / brin / hnsw for real; `USING hash` and `USING gist` are
    // ACCEPTED but backed by the btree kind, and the catalog reports what the
    // index actually IS (btree) rather than claiming an AM SPG doesn't have.
    let rows = alloc::vec![
        Row::new(alloc::vec![
            Value::BigInt(2),
            Value::text("heap"),
            Value::BigInt(0),
            Value::text("t"),
        ]),
        Row::new(alloc::vec![
            Value::BigInt(403),
            Value::text("btree"),
            Value::BigInt(0),
            Value::text("i"),
        ]),
        Row::new(alloc::vec![
            Value::BigInt(405),
            Value::text("hash"),
            Value::BigInt(0),
            Value::text("i"),
        ]),
        Row::new(alloc::vec![
            Value::BigInt(783),
            Value::text("gist"),
            Value::BigInt(0),
            Value::text("i"),
        ]),
        Row::new(alloc::vec![
            Value::BigInt(2742),
            Value::text("gin"),
            Value::BigInt(0),
            Value::text("i"),
        ]),
        Row::new(alloc::vec![
            Value::BigInt(4000),
            Value::text("spgist"),
            Value::BigInt(0),
            Value::text("i"),
        ]),
        Row::new(alloc::vec![
            Value::BigInt(3580),
            Value::text("brin"),
            Value::BigInt(0),
            Value::text("i"),
        ]),
    ];
    (schema, rows)
}

/// v7.37.24 (24.14) — synthesise `pg_catalog.pg_collation`.
/// PG's collation list. ORMs that bind TEXT columns to a
/// language-specific collation read this at handshake to map
/// names → OIDs. SPG ships the three PG-standard collations
/// (`default` / `C` / `POSIX`) — every TEXT column uses
/// `default` so column-level COLLATE clauses parse but don't
/// alter sort order. v7.37.x doesn't yet support per-locale
/// ICU collations; the view shape lets monitoring queries +
/// pg_dump's COLLATE-restoration query both resolve.
pub(crate) fn synth_pg_collation(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("collname", DataType::Text, false),
        ColumnSchema::new("collnamespace", DataType::BigInt, false),
        ColumnSchema::new("collowner", DataType::BigInt, false),
        ColumnSchema::new("collprovider", DataType::Text, false), // 'b'/'c'/'i'
        ColumnSchema::new("collisdeterministic", DataType::Bool, false),
        ColumnSchema::new("collencoding", DataType::Int, false),
        ColumnSchema::new("collcollate", DataType::Text, true),
        ColumnSchema::new("collctype", DataType::Text, true),
    ];
    // PG hard-codes OIDs 100 = default, 950 = C, 951 = POSIX.
    let rows = alloc::vec![
        Row::new(alloc::vec![
            Value::BigInt(100),
            Value::text("default"),
            Value::BigInt(11),
            Value::BigInt(10),
            Value::text("d"),
            Value::Bool(true),
            Value::Int(-1),
            Value::Null,
            Value::Null,
        ]),
        Row::new(alloc::vec![
            Value::BigInt(950),
            Value::text("C"),
            Value::BigInt(11),
            Value::BigInt(10),
            Value::text("c"),
            Value::Bool(true),
            Value::Int(-1),
            Value::text("C"),
            Value::text("C"),
        ]),
        Row::new(alloc::vec![
            Value::BigInt(951),
            Value::text("POSIX"),
            Value::BigInt(11),
            Value::BigInt(10),
            Value::text("c"),
            Value::Bool(true),
            Value::Int(-1),
            Value::text("POSIX"),
            Value::text("POSIX"),
        ]),
    ];
    (schema, rows)
}

/// v7.37.22 (22.17) — synthesise `pg_catalog.pg_stat_archiver`.
/// PG monitoring tools poll this single-row view to track WAL
/// archival progress. SPG uses in-process WAL pubsub (see
/// 21.14 carve-out in v7.37.x-complete-roadmap.md), so the
/// archive-side counters stay 0; the view ships shape-stable
/// so dashboards keep parsing.
///
/// PG-canonical columns:
///   * archived_count (BigInt) — successfully archived files
///   * last_archived_wal (Text) — name of last archived file
///   * last_archived_time (TIMESTAMPTZ)
///   * failed_count (BigInt) — failed archive attempts
///   * last_failed_wal (Text)
///   * last_failed_time (TIMESTAMPTZ)
///   * stats_reset (TIMESTAMPTZ)
pub(crate) fn synth_pg_stat_archiver(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("archived_count", DataType::BigInt, false),
        ColumnSchema::new("last_archived_wal", DataType::Text, true),
        ColumnSchema::new("last_archived_time", DataType::Timestamptz, true),
        ColumnSchema::new("failed_count", DataType::BigInt, false),
        ColumnSchema::new("last_failed_wal", DataType::Text, true),
        ColumnSchema::new("last_failed_time", DataType::Timestamptz, true),
        ColumnSchema::new("stats_reset", DataType::Timestamptz, true),
    ];
    let rows = alloc::vec![Row::new(alloc::vec![
        Value::BigInt(0),
        Value::Null,
        Value::Null,
        Value::BigInt(0),
        Value::Null,
        Value::Null,
        Value::Null,
    ])];
    (schema, rows)
}

/// v7.37.21 (21.13-d) — synthesise `pg_catalog.pg_stat_replication`.
/// One row per active streaming subscriber. SPG's MAGIC_SUB uses
/// in-process change-stream channels rather than a separate WAL
/// sender process; the view shape-stable empties for now and the
/// row set lights up when v7.37.21 wires sender-side state.
///
/// PG-canonical columns (subset that monitoring tools poll):
///   * pid (Int)
///   * usename (Text)
///   * application_name (Text)
///   * client_addr (Text) — PG renders as inet; SPG renders as text
///   * state (Text) — 'startup' / 'catchup' / 'streaming' / 'backup'
///   * sent_lsn / write_lsn / flush_lsn / replay_lsn (Text, PG LSN format)
///   * sync_state (Text) — 'async' / 'potential' / 'sync' / 'quorum'
///   * reply_time (TIMESTAMPTZ)
pub(crate) fn synth_pg_stat_replication(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("pid", DataType::Int, false),
        ColumnSchema::new("usename", DataType::Text, true),
        ColumnSchema::new("application_name", DataType::Text, true),
        ColumnSchema::new("client_addr", DataType::Text, true),
        ColumnSchema::new("state", DataType::Text, false),
        ColumnSchema::new("sent_lsn", DataType::Text, true),
        ColumnSchema::new("write_lsn", DataType::Text, true),
        ColumnSchema::new("flush_lsn", DataType::Text, true),
        ColumnSchema::new("replay_lsn", DataType::Text, true),
        ColumnSchema::new("sync_state", DataType::Text, false),
        ColumnSchema::new("reply_time", DataType::Timestamptz, true),
    ];
    let rows: Vec<Row<'static>> = Vec::new();
    (schema, rows)
}

/// v7.37.22 (22.16) — synthesise `pg_catalog.pg_stat_bgwriter`.
/// PG dashboards poll this single-row view to track background-
/// writer dirty-page churn. SPG's freezer / flusher does the
/// equivalent work; this view reports SPG's freezer-side
/// counters under PG-canonical column names.
///
/// PG-canonical columns:
///   * checkpoints_timed (BigInt)
///   * checkpoints_req (BigInt)
///   * checkpoint_write_time / checkpoint_sync_time (Float, ms)
///   * buffers_checkpoint / buffers_clean / buffers_backend /
///     buffers_backend_fsync / buffers_alloc (BigInt)
///   * maxwritten_clean (BigInt)
///   * stats_reset (TIMESTAMPTZ)
/// v7.38 (read01 P3.15) — `pg_catalog.pg_stat_slru`. SPG has no SLRU
/// caches (its tiered storage is a different subsystem), so the view is
/// empty; the PG columns are present so monitoring queries parse.
pub(crate) fn synth_pg_stat_slru(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("name", DataType::Text, false),
        ColumnSchema::new("blks_zeroed", DataType::BigInt, false),
        ColumnSchema::new("blks_hit", DataType::BigInt, false),
        ColumnSchema::new("blks_read", DataType::BigInt, false),
        ColumnSchema::new("blks_written", DataType::BigInt, false),
        ColumnSchema::new("blks_exists", DataType::BigInt, false),
        ColumnSchema::new("flushes", DataType::BigInt, false),
        ColumnSchema::new("truncates", DataType::BigInt, false),
        ColumnSchema::new("stats_reset", DataType::Timestamptz, true),
    ];
    (schema, Vec::new())
}

/// v7.38 (read01 P3.15) — `pg_catalog.pg_stat_subscription_stats`. One
/// row per subscription would carry apply/sync error + conflict counts;
/// SPG doesn't track them yet, so this is an empty shape-stable shell.
pub(crate) fn synth_pg_stat_subscription_stats(
    _cat: &Catalog,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("subid", DataType::BigInt, false),
        ColumnSchema::new("subname", DataType::Text, false),
        ColumnSchema::new("apply_error_count", DataType::BigInt, false),
        ColumnSchema::new("sync_error_count", DataType::BigInt, false),
        ColumnSchema::new("confl_insert_exists", DataType::BigInt, false),
        ColumnSchema::new("confl_update_origin_differs", DataType::BigInt, false),
        ColumnSchema::new("confl_update_exists", DataType::BigInt, false),
        ColumnSchema::new("confl_update_missing", DataType::BigInt, false),
        ColumnSchema::new("confl_delete_origin_differs", DataType::BigInt, false),
        ColumnSchema::new("confl_delete_missing", DataType::BigInt, false),
        ColumnSchema::new("confl_multiple_unique_conflicts", DataType::BigInt, false),
        ColumnSchema::new("stats_reset", DataType::Timestamptz, true),
    ];
    (schema, Vec::new())
}

/// v7.38 (read01 P3.14) — `pg_catalog.pg_stat_checkpointer` (PG 17+).
/// SPG checkpoints WAL/segments on its own schedule; the cumulative
/// counters aren't wired yet, so this is a shape-stable single row of
/// zeros so monitoring queries parse.
pub(crate) fn synth_pg_stat_checkpointer(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("num_timed", DataType::BigInt, false),
        ColumnSchema::new("num_requested", DataType::BigInt, false),
        ColumnSchema::new("num_done", DataType::BigInt, false),
        ColumnSchema::new("restartpoints_timed", DataType::BigInt, false),
        ColumnSchema::new("restartpoints_req", DataType::BigInt, false),
        ColumnSchema::new("restartpoints_done", DataType::BigInt, false),
        ColumnSchema::new("write_time", DataType::Float, false),
        ColumnSchema::new("sync_time", DataType::Float, false),
        ColumnSchema::new("buffers_written", DataType::BigInt, false),
        ColumnSchema::new("slru_written", DataType::BigInt, false),
        ColumnSchema::new("stats_reset", DataType::Timestamptz, true),
    ];
    let rows = alloc::vec![Row::new(alloc::vec![
        Value::BigInt(0),
        Value::BigInt(0),
        Value::BigInt(0),
        Value::BigInt(0),
        Value::BigInt(0),
        Value::BigInt(0),
        Value::Float(0.0),
        Value::Float(0.0),
        Value::BigInt(0),
        Value::BigInt(0),
        Value::Null,
    ])];
    (schema, rows)
}

/// v7.38 (read01 P3.14) — `pg_catalog.pg_stat_wal`. Shell view; the WAL
/// throughput counters aren't wired yet, so a shape-stable single row of
/// zeros (monitoring queries parse; `stats_reset` is NULL).
pub(crate) fn synth_pg_stat_wal(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("wal_records", DataType::BigInt, false),
        ColumnSchema::new("wal_fpi", DataType::BigInt, false),
        ColumnSchema::new("wal_bytes", DataType::BigInt, false),
        ColumnSchema::new("wal_buffers_full", DataType::BigInt, false),
        ColumnSchema::new("stats_reset", DataType::Timestamptz, true),
    ];
    let rows = alloc::vec![Row::new(alloc::vec![
        Value::BigInt(0),
        Value::BigInt(0),
        Value::BigInt(0),
        Value::BigInt(0),
        Value::Null,
    ])];
    (schema, rows)
}

pub(crate) fn synth_pg_stat_bgwriter(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("checkpoints_timed", DataType::BigInt, false),
        ColumnSchema::new("checkpoints_req", DataType::BigInt, false),
        ColumnSchema::new("checkpoint_write_time", DataType::Float, false),
        ColumnSchema::new("checkpoint_sync_time", DataType::Float, false),
        ColumnSchema::new("buffers_checkpoint", DataType::BigInt, false),
        ColumnSchema::new("buffers_clean", DataType::BigInt, false),
        ColumnSchema::new("maxwritten_clean", DataType::BigInt, false),
        ColumnSchema::new("buffers_backend", DataType::BigInt, false),
        ColumnSchema::new("buffers_backend_fsync", DataType::BigInt, false),
        ColumnSchema::new("buffers_alloc", DataType::BigInt, false),
        ColumnSchema::new("stats_reset", DataType::Timestamptz, true),
    ];
    let rows = alloc::vec![Row::new(alloc::vec![
        Value::BigInt(0), // checkpoints_timed
        Value::BigInt(0), // checkpoints_req
        Value::Float(0.0),
        Value::Float(0.0),
        Value::BigInt(0),
        Value::BigInt(0),
        Value::BigInt(0),
        Value::BigInt(0),
        Value::BigInt(0),
        Value::BigInt(0),
        Value::Null,
    ])];
    (schema, rows)
}

/// v7.37.23 (23.6-b) — synthesise `pg_catalog.pg_tablespace`.
/// SPG is single-tablespace (see TABLESPACES.md). The view ships
/// the two PG-standard rows (`pg_default` + `pg_global`) so
/// tools that join against `pg_tablespace.oid` for placement
/// queries don't get an empty join result.
pub(crate) fn synth_pg_tablespace(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("spcname", DataType::Text, false),
        ColumnSchema::new("spcowner", DataType::BigInt, false),
        ColumnSchema::new("spcacl", DataType::Text, true),
        ColumnSchema::new("spcoptions", DataType::Text, true),
    ];
    let rows = alloc::vec![
        Row::new(alloc::vec![
            Value::BigInt(1663),
            Value::text("pg_default"),
            Value::BigInt(10),
            Value::Null,
            Value::Null,
        ]),
        Row::new(alloc::vec![
            Value::BigInt(1664),
            Value::text("pg_global"),
            Value::BigInt(10),
            Value::Null,
            Value::Null,
        ]),
    ];
    (schema, rows)
}

/// v7.37.22 (22.15) — synthesise `pg_catalog.pg_stat_user_indexes`.
/// PG monitoring tools poll this to flag unused indexes (idx_scan
/// = 0 over the scrape window) as drop candidates. Per-index
/// counters land with v7.37.17's per-AM probes; the shape ships
/// now so monitoring dashboards keep parsing.
///
/// PG-canonical columns:
///   * relid (BigInt) — owning table OID
///   * indexrelid (BigInt) — index OID
///   * schemaname (Text) — 'public'
///   * relname (Text) — owning table
///   * indexrelname (Text)
///   * idx_scan / idx_tup_read / idx_tup_fetch (BigInt) —
///     usage counters
pub(crate) fn synth_pg_stat_user_indexes(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("relid", DataType::BigInt, false),
        ColumnSchema::new("indexrelid", DataType::BigInt, false),
        ColumnSchema::new("schemaname", DataType::Text, false),
        ColumnSchema::new("relname", DataType::Text, false),
        ColumnSchema::new("indexrelname", DataType::Text, false),
        ColumnSchema::new("idx_scan", DataType::BigInt, false),
        ColumnSchema::new("idx_tup_read", DataType::BigInt, false),
        ColumnSchema::new("idx_tup_fetch", DataType::BigInt, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    let mut relid: i64 = 16384;
    let mut indexrelid: i64 = 100_000;
    for tname in cat.visible_table_names() {
        if crate::is_internal_table_name(&tname) {
            continue;
        }
        let Some(t) = cat.get(&tname) else {
            continue;
        };
        for idx in t.indices() {
            indexrelid = indexrelid.saturating_add(1);
            rows.push(Row::new(alloc::vec![
                Value::BigInt(relid),
                Value::BigInt(indexrelid),
                Value::text("public"),
                Value::Text(alloc::borrow::Cow::Owned(tname.clone())),
                Value::Text(alloc::borrow::Cow::Owned(idx.name.clone())),
                Value::BigInt(0), // idx_scan
                Value::BigInt(0), // idx_tup_read
                Value::BigInt(0), // idx_tup_fetch
            ]));
        }
        relid = relid.saturating_add(1);
    }
    (schema, rows)
}

/// v7.37.22 (22.14) — synthesise `pg_catalog.pg_stat_user_tables`.
/// PG monitoring tools poll this per-table view to track row
/// churn (seq vs index scans, tup_ins/upd/del) and surface tables
/// that are candidates for ANALYZE or autovacuum.
///
/// PG-canonical columns (subset that monitoring tools actually
/// consume; the full PG 18 view has 24 columns and the omitted
/// ones are deprecated or per-column instrumented internals):
///   * relid (BigInt) — table OID
///   * schemaname (Text) — 'public'
///   * relname (Text)
///   * seq_scan (BigInt) — sequential-scan count
///   * seq_tup_read (BigInt) — rows read via seqscan
///   * idx_scan (BigInt) — index-scan count
///   * idx_tup_fetch (BigInt) — rows fetched via index
///   * n_tup_ins / n_tup_upd / n_tup_del (BigInt) — row write
///     counters
///   * n_live_tup / n_dead_tup (BigInt) — live + dead row
///     estimates (live = row_count; dead = 0 until v7.37.15
///     vacuum daemon tracks them)
///   * last_vacuum / last_analyze (TIMESTAMPTZ, NULL)
pub(crate) fn synth_pg_stat_user_tables(
    cat: &Catalog,
    write_stats: &alloc::collections::BTreeMap<alloc::string::String, (u64, u64, u64)>,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("relid", DataType::BigInt, false),
        ColumnSchema::new("schemaname", DataType::Text, false),
        ColumnSchema::new("relname", DataType::Text, false),
        ColumnSchema::new("seq_scan", DataType::BigInt, false),
        ColumnSchema::new("seq_tup_read", DataType::BigInt, false),
        ColumnSchema::new("idx_scan", DataType::BigInt, false),
        ColumnSchema::new("idx_tup_fetch", DataType::BigInt, false),
        ColumnSchema::new("n_tup_ins", DataType::BigInt, false),
        ColumnSchema::new("n_tup_upd", DataType::BigInt, false),
        ColumnSchema::new("n_tup_del", DataType::BigInt, false),
        ColumnSchema::new("n_live_tup", DataType::BigInt, false),
        ColumnSchema::new("n_dead_tup", DataType::BigInt, false),
        ColumnSchema::new("last_vacuum", DataType::Timestamptz, true),
        ColumnSchema::new("last_autovacuum", DataType::Timestamptz, true),
        ColumnSchema::new("last_analyze", DataType::Timestamptz, true),
        ColumnSchema::new("last_autoanalyze", DataType::Timestamptz, true),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    let mut relid: i64 = 16384; // PG user-relation OID floor
    for name in cat.visible_table_names() {
        if crate::is_internal_table_name(&name) {
            continue;
        }
        let Some(t) = cat.get(&name) else {
            continue;
        };
        // v7.39 (pg_stat knife A) — real counters: live = visible
        // (physical minus tombstoned; counting `rows()` would include
        // the flip's dead versions), dead = the autovacuum counter,
        // writes = the per-table volatile stats the DML dispatcher
        // bumps. seq/idx scan counters await the scan-path
        // instrumentation knife.
        let dead = i64::try_from(t.dead_rows()).unwrap_or(i64::MAX);
        let live_rows = (t.rows().len() as i64).saturating_sub(dead);
        // r192 — engine-side non-transactional counters (the on-table
        // ones vanished in the RC rebase when bumped inside a tx).
        let (ins, upd, del) = write_stats.get(&name).copied().unwrap_or((0, 0, 0));
        // v7.39 (pg_stat knife B) — scan counters (scan_visible +
        // index-seek instrumentation). This synth query itself walks
        // the catalog, not the user tables, so it doesn't self-count.
        use core::sync::atomic::Ordering;
        let sc = t.scan_stats();
        let as_big = |a: &core::sync::atomic::AtomicU64| {
            Value::BigInt(i64::try_from(a.load(Ordering::Relaxed)).unwrap_or(i64::MAX))
        };
        rows.push(Row::new(alloc::vec![
            Value::BigInt(relid),
            Value::text("public"),
            Value::Text(alloc::borrow::Cow::Owned(name)),
            as_big(&sc.seq_scan),
            as_big(&sc.seq_tup_read),
            as_big(&sc.idx_scan),
            as_big(&sc.idx_tup_fetch),
            Value::BigInt(i64::try_from(ins).unwrap_or(i64::MAX)),
            Value::BigInt(i64::try_from(upd).unwrap_or(i64::MAX)),
            Value::BigInt(i64::try_from(del).unwrap_or(i64::MAX)),
            Value::BigInt(live_rows),
            Value::BigInt(dead),
            // v7.39 (pg_stat knife C) — PG's four maintenance stamps.
            // SPG has no manual-VACUUM statement and no autoanalyze
            // daemon, so those two stay NULL.
            Value::Null, // last_vacuum
            t.maintenance_stamps()
                .0
                .map_or(Value::Null, Value::Timestamp),
            t.maintenance_stamps()
                .1
                .map_or(Value::Null, Value::Timestamp),
            Value::Null, // last_autoanalyze
        ]));
        relid = relid.saturating_add(1);
    }
    (schema, rows)
}

/// v7.37.22 (22.x-stat-db) — synthesise `pg_catalog.pg_stat_database`.
/// PG's per-database scrape view that every monitoring tool
/// (pgwatch, pganalyze, Datadog) polls to track per-DB query
/// counts, deadlocks, conflicts, and cache-hit ratios. SPG is
/// single-database so the row set is always exactly one row,
/// surfacing the global engine counters under the `spg` database
/// name PG-targeted dashboards expect.
///
/// PG-canonical columns (subset that monitoring tools actually
/// read; the full PG 18 view has 27 columns and the omitted ones
/// are deprecated or PG-internal):
///   * datid (BigInt) — database OID (16384)
///   * datname (Text) — `spg`
///   * numbackends (Int) — pgwire backend connection count
///   * xact_commit / xact_rollback (BigInt) — committed /
///     rolled-back transaction counters
///   * blks_read / blks_hit (BigInt) — cold-tier reads /
///     hot-tier hits (mirrors per-relation pg_statio
///     aggregated up to DB)
///   * tup_returned / tup_fetched (BigInt) — rows returned
///     across every SELECT / rows physically read
///   * tup_inserted / tup_updated / tup_deleted (BigInt) —
///     write counters
///   * conflicts / deadlocks (BigInt) — replication-conflict
///     count + per-DB deadlock count (always 0 for SPG single-
///     writer; shape-stable so monitoring queries don't break)
///   * temp_files / temp_bytes (BigInt) — disk-spill counters
///     (0 — SPG aggregate spill lands in v7.37.19 (19.15))
///   * blk_read_time / blk_write_time (Float) — accumulated
///     I/O wait time (0 until per-statement timing lands)
pub(crate) fn synth_pg_stat_database(
    eng: &Engine,
    tup_inserted: u64,
    tup_updated: u64,
    tup_deleted: u64,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("datid", DataType::BigInt, false),
        ColumnSchema::new("datname", DataType::Text, false),
        ColumnSchema::new("numbackends", DataType::Int, false),
        ColumnSchema::new("xact_commit", DataType::BigInt, false),
        ColumnSchema::new("xact_rollback", DataType::BigInt, false),
        ColumnSchema::new("blks_read", DataType::BigInt, false),
        ColumnSchema::new("blks_hit", DataType::BigInt, false),
        ColumnSchema::new("tup_returned", DataType::BigInt, false),
        ColumnSchema::new("tup_fetched", DataType::BigInt, false),
        ColumnSchema::new("tup_inserted", DataType::BigInt, false),
        ColumnSchema::new("tup_updated", DataType::BigInt, false),
        ColumnSchema::new("tup_deleted", DataType::BigInt, false),
        // v7.39 (read01 pgstatfuncs.c) — the full PG18 column set in PG's
        // order (deadlocks sits after temp_bytes; the checksum / session /
        // parallel-worker columns follow).
        ColumnSchema::new("conflicts", DataType::BigInt, false),
        ColumnSchema::new("temp_files", DataType::BigInt, false),
        ColumnSchema::new("temp_bytes", DataType::BigInt, false),
        ColumnSchema::new("deadlocks", DataType::BigInt, false),
        ColumnSchema::new("checksum_failures", DataType::BigInt, true),
        ColumnSchema::new("checksum_last_failure", DataType::Timestamptz, true),
        ColumnSchema::new("blk_read_time", DataType::Float, false),
        ColumnSchema::new("blk_write_time", DataType::Float, false),
        ColumnSchema::new("session_time", DataType::Float, false),
        ColumnSchema::new("active_time", DataType::Float, false),
        ColumnSchema::new("idle_in_transaction_time", DataType::Float, false),
        ColumnSchema::new("sessions", DataType::BigInt, false),
        ColumnSchema::new("sessions_abandoned", DataType::BigInt, false),
        ColumnSchema::new("sessions_fatal", DataType::BigInt, false),
        ColumnSchema::new("sessions_killed", DataType::BigInt, false),
        ColumnSchema::new("parallel_workers_to_launch", DataType::BigInt, false),
        ColumnSchema::new("parallel_workers_launched", DataType::BigInt, false),
        ColumnSchema::new("stats_reset", DataType::Timestamptz, true),
    ];
    // Single-row, single-database; everything reads as 0 until
    // per-counter wiring lands (the shape is stable so monitoring
    // queries parse).
    // v7.39 (pg_stat knife A) — real xact counters + the host's live
    // backend count (embedded: no host slot -> 1, the calling session).
    let commits = eng.xact_commit.load(core::sync::atomic::Ordering::Relaxed);
    let rollbacks = eng
        .xact_rollback
        .load(core::sync::atomic::Ordering::Relaxed);
    let backends = eng.backend_count_fn.map_or(1, |f| f());
    // v7.39 (pg_stat knife C) — tup_returned/tup_fetched aggregate the
    // per-table scan counters (returned ~ rows read by scans of either
    // kind; fetched ~ rows fetched via index scans, PG's split).
    let (mut tup_returned, mut tup_fetched) = (0u64, 0u64);
    {
        use core::sync::atomic::Ordering;
        let cat = eng.active_catalog();
        for name in cat.visible_table_names() {
            if let Some(t) = cat.get(&name) {
                let sc = t.scan_stats();
                tup_returned = tup_returned
                    .saturating_add(sc.seq_tup_read.load(Ordering::Relaxed))
                    .saturating_add(sc.idx_tup_fetch.load(Ordering::Relaxed));
                tup_fetched = tup_fetched.saturating_add(sc.idx_tup_fetch.load(Ordering::Relaxed));
            }
        }
    }
    // v7.39 (pg_stat blks knife) — row-granular block statistics:
    // blks_read = cold-segment row resolutions, blks_hit = hot row
    // accesses (scan reads + index fetches minus the cold ones). The
    // RATIO is what dashboards consume; SPG has no 8 KB page unit.
    let blks_read = eng
        .active_catalog()
        .cold_read_stats
        .cold_reads
        .load(core::sync::atomic::Ordering::Relaxed);
    let blks_hit = tup_returned.saturating_sub(blks_read);
    let rows = alloc::vec![Row::new(alloc::vec![
        Value::BigInt(16384),
        Value::text("spg"),
        Value::Int(i32::try_from(backends).unwrap_or(i32::MAX)),
        Value::BigInt(i64::try_from(commits).unwrap_or(i64::MAX)),
        Value::BigInt(i64::try_from(rollbacks).unwrap_or(i64::MAX)),
        Value::BigInt(i64::try_from(blks_read).unwrap_or(i64::MAX)),
        Value::BigInt(i64::try_from(blks_hit).unwrap_or(i64::MAX)),
        Value::BigInt(i64::try_from(tup_returned).unwrap_or(i64::MAX)),
        Value::BigInt(i64::try_from(tup_fetched).unwrap_or(i64::MAX)),
        Value::BigInt(i64::try_from(tup_inserted).unwrap_or(i64::MAX)),
        Value::BigInt(i64::try_from(tup_updated).unwrap_or(i64::MAX)),
        Value::BigInt(i64::try_from(tup_deleted).unwrap_or(i64::MAX)),
        Value::BigInt(0),  // conflicts (PG: replication-conflict count)
        Value::BigInt(0),  // temp_files (spill; pending 19.15)
        Value::BigInt(0),  // temp_bytes
        Value::BigInt(0),  // deadlocks (SPG single-writer; always 0)
        Value::BigInt(0),  // checksum_failures
        Value::Null,       // checksum_last_failure
        Value::Float(0.0), // blk_read_time
        Value::Float(0.0), // blk_write_time
        Value::Float(0.0), // session_time
        Value::Float(0.0), // active_time
        Value::Float(0.0), // idle_in_transaction_time
        Value::BigInt(0),  // sessions
        Value::BigInt(0),  // sessions_abandoned
        Value::BigInt(0),  // sessions_fatal
        Value::BigInt(0),  // sessions_killed
        Value::BigInt(0),  // parallel_workers_to_launch
        Value::BigInt(0),  // parallel_workers_launched
        Value::Null,       // stats_reset (never reset)
    ])];
    (schema, rows)
}

/// v7.37.21 (21.13-c) — synthesise `pg_catalog.pg_subscription`.
/// One row per CREATE SUBSCRIPTION. Logical-replication tooling
/// (debezium, the PG `pg_stat_subscription` family of views,
/// pgwatch dashboards) reads this surface to inspect the
/// per-subscription connection / publication list / enable
/// state.
///
/// PG-canonical columns (subset; the full view has subconninfo
/// that we omit because it carries the connection-string secret
/// — same security default PG ships with revoking subconninfo
/// for non-superusers):
///   * oid (BigInt)
///   * subdbid (BigInt) — owning database OID
///   * subname (Text)
///   * subowner (BigInt) — 10 (postgres superuser)
///   * subenabled (Bool)
///   * subconninfo (Text) — sanitised to `[redacted]` so
///     dashboards don't accidentally leak credentials when
///     scraping the catalog
///   * subslotname (Text) — the receiver-side slot name
///   * subpublications (Text[] flattened as comma-separated Text)
///   * subbinary (Bool) — false (SPG uses text wire); flips
///     when v7.38 adds binary subscriber wire
///   * substream (Bool) — false (no streaming in-progress txs yet)
pub(crate) fn synth_pg_subscription(eng: &Engine) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("subdbid", DataType::BigInt, false),
        ColumnSchema::new("subname", DataType::Text, false),
        ColumnSchema::new("subowner", DataType::BigInt, false),
        ColumnSchema::new("subenabled", DataType::Bool, false),
        ColumnSchema::new("subconninfo", DataType::Text, false),
        ColumnSchema::new("subslotname", DataType::Text, true),
        ColumnSchema::new("subpublications", DataType::Text, false),
        ColumnSchema::new("subbinary", DataType::Bool, false),
        ColumnSchema::new("substream", DataType::Bool, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    // Subscription OID band starts at 80_000 (publications live
    // at 70_000+, so the two stay disjoint for sub-publication
    // join shapes).
    let mut oid: i64 = 80_000;
    for (name, sub) in eng.subscriptions().iter() {
        oid = oid.saturating_add(1);
        let pubs = sub.publications.join(",");
        rows.push(Row::new(alloc::vec![
            Value::BigInt(oid),
            Value::BigInt(16384), // subdbid (SPG single-db OID)
            Value::text(name.clone()),
            Value::BigInt(10), // subowner
            Value::Bool(sub.enabled),
            Value::text("[redacted]"), // subconninfo
            Value::Null,               // subslotname
            Value::text(pubs),
            Value::Bool(false), // subbinary
            Value::Bool(false), // substream
        ]));
    }
    (schema, rows)
}

/// v7.37.21 (21.13-b) — synthesise `pg_catalog.pg_publication`.
/// One row per CREATE PUBLICATION declaration. Logical-replication
/// subscribers query this at handshake to validate the publication
/// exists + carries the expected table set.
///
/// PG-canonical columns:
///   * oid (BigInt) — publication OID (synthetic, monotonic)
///   * pubname (Text)
///   * pubowner (BigInt) — always 10 (postgres superuser OID)
///   * puballtables (Bool) — true for `FOR ALL TABLES`
///   * pubinsert / pubupdate / pubdelete / pubtruncate (Bool) —
///     SPG publishes all four event types by default; flags
///     surface true to match PG's default scope
///   * pubviaroot (Bool) — partition-parent routing (PG 13+);
///     false for SPG since we route at the engine layer
pub(crate) fn synth_pg_publication(eng: &Engine) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    use spg_sql::ast::PublicationScope;
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("pubname", DataType::Text, false),
        ColumnSchema::new("pubowner", DataType::BigInt, false),
        ColumnSchema::new("puballtables", DataType::Bool, false),
        ColumnSchema::new("pubinsert", DataType::Bool, false),
        ColumnSchema::new("pubupdate", DataType::Bool, false),
        ColumnSchema::new("pubdelete", DataType::Bool, false),
        ColumnSchema::new("pubtruncate", DataType::Bool, false),
        ColumnSchema::new("pubviaroot", DataType::Bool, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    // Synthetic OID band — pubs land above the table OID band
    // (16384..) and above pg_enum's user band (50_000..).
    let mut oid: i64 = 70_000;
    for (name, scope) in eng.publications().iter() {
        oid = oid.saturating_add(1);
        let all_tables = matches!(scope, PublicationScope::AllTables);
        rows.push(Row::new(alloc::vec![
            Value::BigInt(oid),
            Value::text(name.clone()),
            Value::BigInt(10),
            Value::Bool(all_tables),
            Value::Bool(true),  // pubinsert
            Value::Bool(true),  // pubupdate
            Value::Bool(true),  // pubdelete
            Value::Bool(true),  // pubtruncate
            Value::Bool(false), // pubviaroot
        ]));
    }
    (schema, rows)
}

/// v7.37.21 (21.13) — synthesise `pg_catalog.pg_replication_slots`.
/// PG's slot table tracks each physical/logical replication
/// stream's persistent LSN/restart-LSN position so a subscriber
/// reconnecting after a network drop can resume rather than
/// restart from the snapshot.
///
/// SPG's logical replication via MAGIC_SUB doesn't yet persist
/// slot state across engine restarts — every subscriber connects
/// fresh and walks the change-stream from the current LSN. The
/// view ships the PG-canonical columns shape-stable (zero rows)
/// so monitoring queries / dashboards (`SELECT * FROM
/// pg_replication_slots WHERE active = true`) keep parsing
/// against SPG.
///
/// PG-canonical columns:
///   * slot_name (Text)
///   * plugin (Text) — output-plugin name (`pgoutput` etc.)
///   * slot_type (Text) — `physical` / `logical`
///   * datoid (BigInt) — owning database OID
///   * database (Text)
///   * temporary (Bool)
///   * active (Bool)
///   * active_pid (Int)
///   * xmin (BigInt) — oldest tx the slot needs
///   * catalog_xmin (BigInt)
///   * restart_lsn (Text) — PG LSN format ("X/YYYYYYYY")
///   * confirmed_flush_lsn (Text)
///   * wal_status (Text) — `reserved` / `extended` / `unreserved` / `lost`
///   * safe_wal_size (BigInt) — bytes before the slot's WAL is reclaimed
pub(crate) fn synth_pg_replication_slots(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("slot_name", DataType::Text, false),
        ColumnSchema::new("plugin", DataType::Text, true),
        ColumnSchema::new("slot_type", DataType::Text, false),
        ColumnSchema::new("datoid", DataType::BigInt, true),
        ColumnSchema::new("database", DataType::Text, true),
        ColumnSchema::new("temporary", DataType::Bool, false),
        ColumnSchema::new("active", DataType::Bool, false),
        ColumnSchema::new("active_pid", DataType::Int, true),
        ColumnSchema::new("xmin", DataType::BigInt, true),
        ColumnSchema::new("catalog_xmin", DataType::BigInt, true),
        ColumnSchema::new("restart_lsn", DataType::Text, true),
        ColumnSchema::new("confirmed_flush_lsn", DataType::Text, true),
        ColumnSchema::new("wal_status", DataType::Text, true),
        ColumnSchema::new("safe_wal_size", DataType::BigInt, true),
    ];
    // Empty until SPG persists slot state across engine restarts
    // (21.12 dependency). The shape is stable so dashboards keep
    // parsing.
    let rows: Vec<Row<'static>> = Vec::new();
    (schema, rows)
}

/// v7.37.24 (24.3) — synthesise `information_schema.attributes`.
/// PG-standard surface listing every field of every composite
/// type. ORM enum/composite codecs and pg_dump use this to
/// reconstruct composite-type declarations at dump-time.
///
/// PG-canonical columns (subset; full SQL-standard shape is ~28
/// columns, we ship the ones tools actually read at startup):
///   * udt_catalog / udt_schema / udt_name — the composite type
///   * attribute_name — field name
///   * ordinal_position — 1-based field position
///   * data_type — PG-canonical type name of the field
///   * is_nullable — always 'YES' (composite fields default to
///     nullable per SQL-standard; field-level NOT NULL would
///     need a richer CompositeDef)
pub(crate) fn synth_information_schema_attributes(
    cat: &Catalog,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("udt_catalog", DataType::Text, false),
        ColumnSchema::new("udt_schema", DataType::Text, false),
        ColumnSchema::new("udt_name", DataType::Text, false),
        ColumnSchema::new("attribute_name", DataType::Text, false),
        ColumnSchema::new("ordinal_position", DataType::Int, false),
        ColumnSchema::new("data_type", DataType::Text, false),
        ColumnSchema::new("is_nullable", DataType::Text, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for (_name, def) in cat.composite_types() {
        for (i, (field_name, field_type)) in def.fields.iter().enumerate() {
            #[allow(clippy::cast_possible_wrap)]
            let ordinal = (i + 1) as i32;
            rows.push(Row::new(alloc::vec![
                Value::text("spg"),
                Value::text("public"),
                Value::text(def.name.clone()),
                Value::text(field_name.clone()),
                Value::Int(ordinal),
                Value::text(pg_data_type_text(*field_type)),
                Value::text("YES"),
            ]));
        }
    }
    (schema, rows)
}

/// v7.37.24 (24.2) — synthesise `information_schema.domains`.
/// One row per DOMAIN type. PG-targeting tools (Liquibase /
/// Alembic migrations) query this surface to round-trip DOMAIN
/// declarations across the dump/restore cycle.
///
/// PG-canonical columns (SQL-standard):
///   * domain_catalog / domain_schema / domain_name
///   * data_type — PG-canonical type name of the base
///   * udt_catalog / udt_schema / udt_name — underlying type
///   * domain_default — DEFAULT expression text (NULL if none)
///   * is_nullable — 'YES' / 'NO'
pub(crate) fn synth_information_schema_domains(
    cat: &Catalog,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("domain_catalog", DataType::Text, false),
        ColumnSchema::new("domain_schema", DataType::Text, false),
        ColumnSchema::new("domain_name", DataType::Text, false),
        ColumnSchema::new("data_type", DataType::Text, false),
        ColumnSchema::new("udt_catalog", DataType::Text, false),
        ColumnSchema::new("udt_schema", DataType::Text, false),
        ColumnSchema::new("udt_name", DataType::Text, false),
        ColumnSchema::new("domain_default", DataType::Text, true),
        ColumnSchema::new("is_nullable", DataType::Text, false),
    ];
    let rows: Vec<Row<'static>> = cat
        .domain_types()
        .values()
        .map(|def| {
            let base_name = pg_data_type_text(def.base_type);
            Row::new(alloc::vec![
                Value::text("spg"),
                Value::text("public"),
                Value::text(def.name.clone()),
                Value::text(base_name.clone()),
                Value::text("spg"),
                Value::text("pg_catalog"),
                Value::text(base_name),
                def.default
                    .as_ref()
                    .map(|s| Value::text(s.clone()))
                    .unwrap_or(Value::Null),
                Value::text(if def.nullable { "YES" } else { "NO" }),
            ])
        })
        .collect();
    (schema, rows)
}

/// v7.37.24 (24.1) — synthesise `pg_catalog.pg_enum`. One row
/// per (enum_type, label) pair. Tools targeting PG (sqlx ENUM
/// codec, ORM enum mappers, pg_dump's `--enum-by-label` query)
/// read this surface to reconstruct ENUM types at dump-time.
///
/// PG-canonical columns:
///   * oid (BigInt) — per-label OID (synthetic, monotonic)
///   * enumtypid (BigInt) — owning type OID (synthetic, one
///     per CREATE TYPE)
///   * enumsortorder (Float) — 1-based sort position within
///     the enum (matches PG's float4 sort key shape)
///   * enumlabel (Text) — the literal label
pub(crate) fn synth_pg_enum(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("enumtypid", DataType::BigInt, false),
        ColumnSchema::new("enumsortorder", DataType::Float, false),
        ColumnSchema::new("enumlabel", DataType::Text, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    // Synthetic OID bands: enum types start at 50_000; per-label
    // OIDs start at 60_000. Keeps them disjoint from pg_type's
    // user-space scalar OID band (which lives above 16384 but
    // below the 50_000 mark we land enum types into).
    let mut typid: i64 = 50_000;
    let mut label_oid: i64 = 60_000;
    for (_name, def) in cat.enum_types() {
        typid = typid.saturating_add(1);
        for (i, label) in def.labels.iter().enumerate() {
            label_oid = label_oid.saturating_add(1);
            rows.push(Row::new(alloc::vec![
                Value::BigInt(label_oid),
                Value::BigInt(typid),
                Value::Float((i + 1) as f64),
                Value::text(label.clone()),
            ]));
        }
    }
    (schema, rows)
}

/// v7.37.24 (24.9) — synthesise
/// `information_schema.table_constraints`. PG-standard surface
/// listing every PRIMARY KEY / UNIQUE / FOREIGN KEY / CHECK
/// constraint. Migration tools (Liquibase, Alembic) compare
/// before/after snapshots of this view to detect schema drift.
pub(crate) fn synth_information_schema_table_constraints(
    cat: &Catalog,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("constraint_catalog", DataType::Text, false),
        ColumnSchema::new("constraint_schema", DataType::Text, false),
        ColumnSchema::new("constraint_name", DataType::Text, false),
        ColumnSchema::new("table_catalog", DataType::Text, false),
        ColumnSchema::new("table_schema", DataType::Text, false),
        ColumnSchema::new("table_name", DataType::Text, false),
        ColumnSchema::new("constraint_type", DataType::Text, false),
        ColumnSchema::new("is_deferrable", DataType::Text, false),
        ColumnSchema::new("initially_deferred", DataType::Text, false),
        ColumnSchema::new("enforced", DataType::Text, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for tname in cat.visible_table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        // Uniqueness constraints — both PK and UNIQUE forms.
        for uc in t.schema().uniqueness_constraints.iter() {
            let conname = pg_unique_conname(t, uc, &tname);
            let kind = if uc.is_primary_key {
                "PRIMARY KEY"
            } else {
                "UNIQUE"
            };
            rows.push(Row::new(alloc::vec![
                Value::text("spg"),
                Value::text("public"),
                Value::text(conname),
                Value::text("spg"),
                Value::text("public"),
                Value::text(tname.clone()),
                Value::text(kind),
                Value::text("NO"),
                Value::text("NO"),
                Value::text("YES"),
            ]));
        }
        // Single-column unique indices without a UC entry.
        // v7.39 (read01 ruleutils.c) — partial unique indexes are not
        // constraints (PG).
        for idx in t.indices() {
            if !idx.is_unique || idx.partial_predicate.is_some() {
                continue;
            }
            let already = t
                .schema()
                .uniqueness_constraints
                .iter()
                .any(|uc| uc.columns.len() == 1 && uc.columns[0] == idx.column_position);
            if already {
                continue;
            }
            let is_primary = idx.name.ends_with("_pkey");
            let kind = if is_primary { "PRIMARY KEY" } else { "UNIQUE" };
            rows.push(Row::new(alloc::vec![
                Value::text("spg"),
                Value::text("public"),
                Value::text(idx.name.clone()),
                Value::text("spg"),
                Value::text("public"),
                Value::text(tname.clone()),
                Value::text(kind),
                Value::text("NO"),
                Value::text("NO"),
                Value::text("YES"),
            ]));
        }
        // Foreign keys.
        for fk in t.schema().foreign_keys.iter() {
            let conname = fk
                .name
                .clone()
                .unwrap_or_else(|| pg_fk_conname(t, fk, &tname));
            rows.push(Row::new(alloc::vec![
                Value::text("spg"),
                Value::text("public"),
                Value::text(conname),
                Value::text("spg"),
                Value::text("public"),
                Value::text(tname.clone()),
                Value::text("FOREIGN KEY"),
                Value::text("NO"),
                Value::text("NO"),
                Value::text("YES"),
            ]));
        }
        // CHECK constraints — PG-canonical `{table}_{col}_check` names.
        let check_names = pg_check_connames(t, &tname, &t.schema().checks);
        for (ci, _check) in t.schema().checks.iter().enumerate() {
            rows.push(Row::new(alloc::vec![
                Value::text("spg"),
                Value::text("public"),
                Value::text(check_names[ci].clone()),
                Value::text("spg"),
                Value::text("public"),
                Value::text(tname.clone()),
                Value::text("CHECK"),
                Value::text("NO"),
                Value::text("NO"),
                Value::text("YES"),
            ]));
        }
        // v7.38 (read01 P6.37) — PG 18 tracks each NOT NULL column as a CHECK
        // constraint named `{table}_{col}_not_null`, so it appears in
        // table_constraints with constraint_type CHECK.
        for col in t.schema().columns.iter() {
            if col.nullable {
                continue;
            }
            rows.push(Row::new(alloc::vec![
                Value::text("spg"),
                Value::text("public"),
                Value::text(alloc::format!("{tname}_{}_not_null", col.name)),
                Value::text("spg"),
                Value::text("public"),
                Value::text(tname.clone()),
                Value::text("CHECK"),
                Value::text("NO"),
                Value::text("NO"),
                Value::text("YES"),
            ]));
        }
    }
    (schema, rows)
}

/// v7.39 (round 338, V64) — the ONE place a relation's oid is decided.
///
/// Each kind of relation used to number itself wherever it was
/// synthesised, and the numbers disagreed: a sequence was 300_001 in
/// `pg_class` but 32_768 in `pg_sequence`, so PG's canonical
/// `pg_class JOIN pg_sequence ON oid = seqrelid` returned nothing — and
/// 32_768 was simultaneously the *view* band, so a view and a sequence
/// could answer to the same oid. Every synth now reads its band from
/// here, and so does the `::regclass` cast, which is what makes the
/// joins between them line up.
pub(crate) const OID_TABLE_BASE: i64 = 16384;
pub(crate) const OID_VIEW_BASE: i64 = 32768;
pub(crate) const OID_INDEX_BASE: i64 = 100_000;
pub(crate) const OID_SEQ_BASE: i64 = 300_000;
/// v7.39 (round 342, V65) — user functions, keyed by signature the way
/// `pg_proc` iterates them.
pub(crate) const OID_FUNC_BASE: i64 = 400_000;

/// Resolve a function NAME to the oid `pg_proc` gives it. `None` when no
/// function has that name, or when the name is overloaded — an overload
/// needs a signature (`regprocedure`), which is PG's rule too.
pub(crate) fn function_oid(cat: &Catalog, bare: &str) -> Option<i64> {
    let mut oid = OID_FUNC_BASE;
    let mut hit = None;
    for def in cat.functions().values() {
        oid += 1;
        if def.name == bare {
            if hit.is_some() {
                return None;
            }
            hit = Some(oid);
        }
    }
    hit
}

/// The oid of one specific overload, matched by its canonical argument
/// types (`integer,text`).
pub(crate) fn function_oid_by_signature(cat: &Catalog, bare: &str, arg_types: &str) -> Option<i64> {
    let mut oid = OID_FUNC_BASE;
    for def in cat.functions().values() {
        oid += 1;
        if def.name == bare && canonical_arg_types(&def.args_repr) == arg_types {
            return Some(oid);
        }
    }
    None
}

/// Resolve a relation name to the oid every catalog synth uses for it.
/// The iteration order here IS the assignment order the synths replay.
/// v7.39 (round 473) — the inverse of [`relation_oid`].
///
/// `indexrelid::regclass` answered the bare number, because the cast had no
/// catalog to look the oid up in. It does now, and pg_index / pg_class rows
/// are read by joining on oids and rendering them — a tool that printed
/// `100001` where PG prints `ix1` cannot match the two up.
///
/// Deliberately mirrors `relation_oid`'s walks step for step, in the same
/// order over the same lists, so the two cannot disagree about which oid
/// belongs to which relation.
/// v7.39 (round 518) — is this one of the built-in type oids? The
/// visibility probes ask before answering, as PG does.
pub(crate) fn builtin_type_oid_exists(oid: i64) -> bool {
    [
        DataType::Bool,
        DataType::SmallInt,
        DataType::Int,
        DataType::BigInt,
        DataType::Real,
        DataType::Float,
        DataType::Numeric {
            precision: 0,
            scale: 0,
        },
        DataType::Text,
        DataType::Bytes,
        DataType::Date,
        DataType::Timestamp,
        DataType::Timestamptz,
        DataType::Interval,
        DataType::Uuid,
        DataType::Json,
        DataType::Jsonb,
    ]
    .into_iter()
    .any(|t| pg_type_oid(t) == oid)
}

pub(crate) fn relation_name_for_oid(cat: &Catalog, oid: i64) -> Option<String> {
    for (pos, tname) in cat.table_names().iter().enumerate() {
        if OID_TABLE_BASE + pos as i64 == oid {
            return cat.listed_name(tname).map(alloc::string::String::from);
        }
    }
    for (pos, vname) in cat.views_all().keys().enumerate() {
        let Some(vname) = cat.listed_name(vname) else {
            continue;
        };
        if OID_VIEW_BASE + pos as i64 == oid {
            return Some(alloc::string::String::from(vname));
        }
    }
    let mut idx_oid = OID_INDEX_BASE;
    for tname in cat.visible_table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        for idx in t.indices() {
            idx_oid += 1;
            if idx_oid == oid {
                return Some(idx.name.clone());
            }
        }
    }
    let mut seq_oid = OID_SEQ_BASE;
    for name in cat.sequences_all().keys() {
        let Some(name) = cat.listed_name(name) else {
            continue;
        };
        seq_oid += 1;
        if seq_oid == oid {
            return Some(alloc::string::String::from(name));
        }
    }
    None
}

pub(crate) fn relation_oid(cat: &Catalog, bare: &str) -> Option<i64> {
    // v7.39 (round 437) — the RAW list, because that is the order the synths
    // assign oids in (a foreign session's temporary table still consumes
    // its slot, it is only left out of the OUTPUT). The name is matched
    // through the session's temp namespace, so `tmp` finds the caller's own
    // temporary table at its real catalog position.
    let stored = cat.temp_name_for(bare);
    for (pos, tname) in cat.table_names().iter().enumerate() {
        if tname == bare || Some(tname) == stored.as_ref() {
            return Some(OID_TABLE_BASE + pos as i64);
        }
    }
    for (pos, vname) in cat.views_all().keys().enumerate() {
        let Some(vname) = cat.listed_name(vname) else {
            continue;
        };
        if vname == bare {
            return Some(OID_VIEW_BASE + pos as i64);
        }
    }
    let mut idx_oid = OID_INDEX_BASE;
    for tname in cat.visible_table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        for idx in t.indices() {
            idx_oid += 1;
            if idx.name == bare {
                return Some(idx_oid);
            }
        }
    }
    let mut seq_oid = OID_SEQ_BASE;
    for name in cat.sequences_all().keys() {
        let Some(name) = cat.listed_name(name) else {
            continue;
        };
        seq_oid += 1;
        if name == bare {
            return Some(seq_oid);
        }
    }
    None
}

/// v7.16.2 + v7.37.24 (24.8) — synthesise `pg_catalog.pg_class`.
/// Widened to cover the columns dashboards / monitoring tools
/// query (relkind, reltuples for size estimates, relnatts for
/// column count, relhasindex flag, relpersistence, relispartition
/// for partition awareness). PG18's pg_class has ~30 columns;
/// the subset here is "every column an external tool actually
/// reads against SPG" — additional columns land as we observe
/// new tools query them.
/// v7.39 (round 526) — the schema an oid names, for `::regnamespace`.
/// The same three `pg_namespace` publishes, so a join on `relnamespace`
/// and the cast agree.
#[must_use]
pub(crate) fn schema_name_for_oid(oid: i64) -> Option<alloc::string::String> {
    let name = match oid {
        11 => "pg_catalog",
        2200 => "public",
        13000 => "information_schema",
        _ => return None,
    };
    Some(alloc::string::String::from(name))
}

pub(crate) fn synth_pg_class(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    use spg_storage::PartitionRole;
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("relname", DataType::Text, false),
        ColumnSchema::new("relnamespace", DataType::BigInt, false),
        ColumnSchema::new("reltype", DataType::BigInt, false),
        ColumnSchema::new("reloftype", DataType::BigInt, false),
        ColumnSchema::new("relowner", DataType::BigInt, false),
        ColumnSchema::new("relam", DataType::BigInt, false),
        ColumnSchema::new("relfilenode", DataType::BigInt, false),
        ColumnSchema::new("reltablespace", DataType::BigInt, false),
        ColumnSchema::new("relpages", DataType::Int, false),
        ColumnSchema::new("reltuples", DataType::Float, false),
        ColumnSchema::new("relallvisible", DataType::Int, false),
        ColumnSchema::new("reltoastrelid", DataType::BigInt, false),
        ColumnSchema::new("relhasindex", DataType::Bool, false),
        ColumnSchema::new("relisshared", DataType::Bool, false),
        ColumnSchema::new("relpersistence", DataType::Text, false),
        ColumnSchema::new("relkind", DataType::Text, false),
        ColumnSchema::new("relnatts", DataType::SmallInt, false),
        ColumnSchema::new("relchecks", DataType::SmallInt, false),
        ColumnSchema::new("relhasrules", DataType::Bool, false),
        ColumnSchema::new("relhastriggers", DataType::Bool, false),
        ColumnSchema::new("relhassubclass", DataType::Bool, false),
        ColumnSchema::new("relrowsecurity", DataType::Bool, false),
        ColumnSchema::new("relforcerowsecurity", DataType::Bool, false),
        ColumnSchema::new("relispopulated", DataType::Bool, false),
        ColumnSchema::new("relreplident", DataType::Text, false),
        ColumnSchema::new("relispartition", DataType::Bool, false),
        // v7.39 (read01 round 57) — PG leaves relacl NULL while only the
        // owner's implicit privileges apply, and materialises the aclitem
        // array on the first GRANT. SPG now does the same for real (round 51
        // hard-coded the NULL, because GRANT was a no-op).
        ColumnSchema::new("relacl", DataType::Text, true),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    // PG starts user-relation OIDs above 16384.
    // v7.39 (round 437) — walk the RAW list so an OID stays tied to a
    // table's catalog position, which is exactly what `relation_oid`
    // replays. Another session's temporary table still consumes its oid;
    // it is only skipped from the OUTPUT.
    for (pos, stored) in cat.table_names().into_iter().enumerate() {
        let this_oid = OID_TABLE_BASE + pos as i64;
        let Some(tname) = cat.listed_name(&stored).map(alloc::string::String::from) else {
            continue;
        };
        let Some(t) = cat.get(&tname) else { continue };
        let is_temp = stored != tname;
        let schema_ref = t.schema();
        // v7.39 (round 338, V64) — a MATERIALIZED VIEW is backed by a real
        // table in SPG, and that showed through: it reported relkind 'r',
        // so a tool listing `WHERE relkind = 'm'` found none of them and a
        // migration tool would recreate it as a plain table.
        let relkind: &'static str = if cat.materialized_views().contains_key(&tname) {
            "m"
        } else {
            match &schema_ref.partition_role {
                Some(PartitionRole::Parent { .. }) => "p", // partitioned table
                _ => "r",                                  // regular table
            }
        };
        let is_partition = matches!(
            &schema_ref.partition_role,
            Some(PartitionRole::Range { .. })
                | Some(PartitionRole::List { .. })
                | Some(PartitionRole::Hash { .. })
                | Some(PartitionRole::Default { .. })
        );
        let relnatts = i16::try_from(schema_ref.columns.len()).unwrap_or(i16::MAX);
        let reltuples = t.rows().len() as f64;
        // relpages in PG-page units (8 KiB) off the maintained
        // hot-tier byte meter — capacity queries multiply
        // relpages × 8192 to estimate table size.
        let relpages = i32::try_from(t.hot_bytes().div_ceil(8192)).unwrap_or(i32::MAX);
        let has_index = !t.indices().is_empty();
        let has_triggers = cat
            .triggers()
            .iter()
            .any(|tr| tr.table.eq_ignore_ascii_case(&tname));
        let has_checks = schema_ref.checks.len();
        rows.push(Row::new(alloc::vec![
            Value::BigInt(this_oid),
            Value::text(tname.clone()),
            Value::BigInt(2200),  // public namespace
            Value::BigInt(0),     // reltype (composite type OID; SPG no composite)
            Value::BigInt(0),     // reloftype
            Value::BigInt(10),    // relowner — PG postgres superuser OID
            Value::BigInt(0),     // relam (table AM; 0 == default heap)
            Value::BigInt(this_oid),   // relfilenode shares oid in SPG (no separate fork)
            Value::BigInt(0),     // reltablespace (0 == default)
            Value::Int(relpages), // hot_bytes in 8 KiB PG-page units
            Value::Float(reltuples),
            Value::Int(0),    // relallvisible — visibility map lands in 15.17
            Value::BigInt(0), // reltoastrelid (SPG no TOAST)
            Value::Bool(has_index),
            Value::Bool(false), // relisshared
            // v7.39 (round 526) — 't' for a TEMPORARY relation. This was
            // pinned 'p', so every temp table, view and sequence reported
            // itself permanent: a tool listing temp objects found none,
            // and one deciding what to dump saw them as dumpable. The
            // witness is the one the resolver already uses — a temp
            // relation is stored under a session-prefixed name, so its
            // stored and listed names differ.
            Value::text(if is_temp { "t" } else { "p" }),
            Value::text(relkind),
            Value::SmallInt(relnatts),
            Value::SmallInt(i16::try_from(has_checks).unwrap_or(i16::MAX)),
            // v7.39 (round 338, V64) — relhasrules for real. It was pinned
            // false with the note "SPG has no rule system", which stopped
            // being true when CREATE RULE landed; a matview carries its
            // _RETURN rule in PG, so it reads true there too.
            Value::Bool(
                relkind == "m"
                    || cat
                        .rules()
                        .iter()
                        .any(|r| r.table.eq_ignore_ascii_case(&tname)),
            ),
            Value::Bool(has_triggers),
            Value::Bool(false),                         // relhassubclass
            Value::Bool(schema_ref.row_security),       // relrowsecurity (v7.39 RLS)
            Value::Bool(schema_ref.force_row_security), // relforcerowsecurity
            Value::Bool(true),                          // relispopulated
            Value::text("d"),                           // relreplident — 'd' default
            Value::Bool(is_partition),
            // v7.39 (read01 round 57) — relacl for real: NULL while no GRANT
            // has ever run, then the aclitem array PG prints.
            crate::acl::render_relacl(schema_ref).map_or(Value::Null, Value::text),
        ]));
    }
    // v7.39 (round 338, V64) — a row PER VIEW (relkind 'v'). pg_class had
    // NO view rows at all: `SELECT … FROM pg_class WHERE relname = '<view>'`
    // came back empty, `WHERE relkind = 'v'` listed none, and every
    // pg_class-anchored join for a view (pg_attribute, pg_rewrite,
    // relacl lookups) dead-ended. PG's values, measured: relam 0,
    // relfilenode 0 (a view has no storage), relpages 0, reltuples -1,
    // relhasrules TRUE (the _RETURN rule), relreplident 'n'.
    for stored in cat.views_all().keys() {
        let Some(vname) = cat.listed_name(stored) else {
            continue;
        };
        // v7.39 (round 526) — a temp relation is stored under a
        // session-prefixed name; that is the same witness the resolver uses.
        let is_temp = stored != vname;
        let Some(view_oid) = relation_oid(cat, vname) else {
            continue;
        };
        let relnatts = i16::try_from(crate::describe::describe_view_columns(cat, vname).len())
            .unwrap_or(i16::MAX);
        rows.push(Row::new(alloc::vec![
            Value::BigInt(view_oid),
            Value::text(vname.to_string()),
            Value::BigInt(2200), // relnamespace — public
            Value::BigInt(0),    // reltype
            Value::BigInt(0),    // reloftype
            Value::BigInt(10),   // relowner
            Value::BigInt(0),    // relam — a view has no access method
            Value::BigInt(0),    // relfilenode — nor any storage
            Value::BigInt(0),
            Value::Int(0),      // relpages
            Value::Float(-1.0), // reltuples — -1 = never analysed
            Value::Int(0),
            Value::BigInt(0),
            Value::Bool(false), // relhasindex
            Value::Bool(false),
            Value::text(if is_temp { "t" } else { "p" }),
            Value::text("v"), // relkind — view
            Value::SmallInt(relnatts),
            Value::SmallInt(0),
            Value::Bool(true),  // relhasrules — the _RETURN rule
            Value::Bool(false), // relhastriggers
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(true),  // relispopulated
            Value::text("n"),   // relreplident — 'n' for a view
            Value::Bool(false), // relispartition
            Value::Null,        // relacl
        ]));
    }
    // v7.39 (read01 round 53) — pg_class also holds a row PER INDEX
    // (relkind 'i'), which is what makes PG's canonical
    // `pg_class JOIN pg_index ON indexrelid = oid JOIN pg_am ON relam = am.oid`
    // join resolve. It used to emit tables only, so that join — the one psql
    // \d and every ORM use to learn an index's access method — came back empty.
    // Index oids follow the SAME sequence synth_pg_index_raw uses (from
    // 100_000, tables in table_names() order, indices in catalog order), so
    // indexrelid and pg_class.oid agree.
    let mut idx_oid: i64 = OID_INDEX_BASE;
    for tname in cat.visible_table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        for idx in t.indices() {
            idx_oid += 1;
            let relnatts = i16::try_from(1 + idx.extra_column_positions.len()).unwrap_or(i16::MAX);
            rows.push(Row::new(alloc::vec![
                Value::BigInt(idx_oid),
                Value::text(idx.name.clone()),
                Value::BigInt(2200),                 // relnamespace — public
                Value::BigInt(0),                    // reltype (indexes have none)
                Value::BigInt(0),                    // reloftype
                Value::BigInt(10),                   // relowner
                Value::BigInt(am_oid_of(&idx.kind)), // relam — the real AM
                Value::BigInt(idx_oid),
                Value::BigInt(0),
                Value::Int(0),
                Value::Float(0.0),
                Value::Int(0),
                Value::BigInt(0),
                Value::Bool(false), // relhasindex (an index has none)
                Value::Bool(false),
                Value::text("p"),
                Value::text("i"), // relkind — index
                Value::SmallInt(relnatts),
                Value::SmallInt(0),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(true),
                Value::text("n"), // relreplident — 'n' for an index
                Value::Bool(false),
                Value::Null,
            ]));
        }
    }
    // v7.39 (read01 round 60) — and a row PER SEQUENCE (relkind 'S'). Sequences
    // were missing from pg_class entirely, so `SELECT relacl FROM pg_class WHERE
    // relname = '<seq>'` — the canonical way to read a sequence's privileges —
    // came back empty.
    for (stored, def) in cat.sequences_all() {
        let Some(name) = cat.listed_name(stored) else {
            continue;
        };
        // v7.39 (round 526) — same witness as the table loop.
        let is_temp = stored != name;
        let Some(seq_oid) = relation_oid(cat, name) else {
            continue;
        };
        rows.push(Row::new(alloc::vec![
            Value::BigInt(seq_oid),
            Value::text(name.to_string()),
            Value::BigInt(2200), // relnamespace — public
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(10), // relowner
            Value::BigInt(0),  // relam — a sequence has no access method
            Value::BigInt(seq_oid),
            Value::BigInt(0),
            Value::Int(1),     // relpages — a sequence is one page
            Value::Float(1.0), // reltuples — and one tuple
            Value::Int(0),
            Value::BigInt(0),
            Value::Bool(false), // relhasindex
            Value::Bool(false),
            Value::text(if is_temp { "t" } else { "p" }),
            Value::text("S"), // relkind — SEQUENCE
            Value::SmallInt(3),
            Value::SmallInt(0),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(true),
            Value::text("n"),
            Value::Bool(false),
            crate::acl::render_acl_list(&def.acl).map_or(Value::Null, Value::text),
        ]));
    }
    (schema, rows)
}

/// v7.16.2 + v7.37.24 (24.8b) — synthesise `pg_catalog.pg_attribute`.
/// Widened from 5 to 16 PG-canonical columns to cover what
/// dashboard / ORM-introspection tools query: column type id +
/// length + nullability + default-presence + identity/generated
/// + array dimensions + collation. Tools doing
/// `SELECT * FROM pg_attribute WHERE attrelid = …::regclass`
/// see the same shape they'd see against PG.
pub(crate) fn synth_pg_attribute(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("attrelid", DataType::BigInt, false),
        ColumnSchema::new("attname", DataType::Text, false),
        ColumnSchema::new("atttypid", DataType::BigInt, false),
        ColumnSchema::new("attstattarget", DataType::Int, false),
        ColumnSchema::new("attlen", DataType::SmallInt, false),
        ColumnSchema::new("attnum", DataType::SmallInt, false),
        ColumnSchema::new("attndims", DataType::Int, false),
        ColumnSchema::new("atttypmod", DataType::Int, false),
        ColumnSchema::new("attbyval", DataType::Bool, false),
        ColumnSchema::new("attstorage", DataType::Text, false),
        ColumnSchema::new("attalign", DataType::Text, false),
        ColumnSchema::new("attnotnull", DataType::Bool, false),
        ColumnSchema::new("atthasdef", DataType::Bool, false),
        ColumnSchema::new("attidentity", DataType::Text, false),
        ColumnSchema::new("attgenerated", DataType::Text, false),
        ColumnSchema::new("attisdropped", DataType::Bool, false),
        ColumnSchema::new("attislocal", DataType::Bool, false),
        ColumnSchema::new("attinhcount", DataType::Int, false),
        ColumnSchema::new("attcollation", DataType::BigInt, false),
        // v7.39 (read01 round 59) — column-level privileges. NULL until a
        // `GRANT SELECT (col)` lands; a column grant never touches relacl.
        ColumnSchema::new("attacl", DataType::Text, true),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    let mut attrelid: i64 = 16384;
    for tname in cat.visible_table_names() {
        let Some(t) = cat.get(&tname) else {
            attrelid = attrelid.saturating_add(1);
            continue;
        };
        for (i, col) in t.schema().columns.iter().enumerate() {
            #[allow(clippy::cast_possible_wrap)]
            let attnum = (i + 1) as i16;
            // PG: typlen — fixed-width width in bytes; -1 for var-length.
            let typlen: i16 = pg_type_len(col.ty);
            // attndims — number of array dimensions. Most array
            // types are 1-D in SPG; jagged / 2-D arrays report 2.
            let attndims: i32 = match col.ty {
                DataType::TextArray
                | DataType::IntArray
                | DataType::BigIntArray
                | DataType::SmallIntArray
                | DataType::FloatArray
                | DataType::BoolArray
                | DataType::DateArray
                | DataType::TimestampArray
                | DataType::TimestamptzArray
                | DataType::UuidArray
                | DataType::BytesArray
                | DataType::NumericArray
                | DataType::JsonArray => 1,
                DataType::IntArray2D | DataType::BigIntArray2D | DataType::TextArray2D => 2,
                _ => 0,
            };
            // attstorage — 'p' plain (fixed-width), 'e' external,
            // 'm' main-or-toast, 'x' extended (default for varlena).
            // SPG's hot tier stores everything in-line; PG-style
            // approximation: fixed-width = 'p', variable = 'x'.
            let attstorage = if typlen > 0 { "p" } else { "x" };
            // attalign — 'c' char, 's' short, 'i' int, 'd' double.
            let attalign = match typlen {
                1 => "c",
                2 => "s",
                4 => "i",
                _ => "d",
            };
            let has_default = col.default.is_some() || col.runtime_default.is_some();
            // attidentity — '' (none), 'a' ALWAYS, 'd' BY DEFAULT.
            // SPG treats auto_increment as identity-default.
            let attidentity = if col.auto_increment { "d" } else { "" };
            rows.push(Row::new(alloc::vec![
                Value::BigInt(attrelid),
                Value::text(col.name.clone()),
                Value::BigInt(pg_type_oid(col.ty)),
                Value::Int(-1), // attstattarget — -1 = use system default
                Value::SmallInt(typlen),
                Value::SmallInt(attnum),
                Value::Int(attndims),
                Value::Int(-1), // atttypmod — -1 = no modifier
                Value::Bool(typlen > 0 && typlen <= 8),
                Value::text(attstorage),
                Value::text(attalign),
                Value::Bool(!col.nullable),
                Value::Bool(has_default),
                Value::text(attidentity),
                // v7.38 (read01 P6.41) — 's' for a STORED generated column
                // (SPG stores generated columns as STORED), '' otherwise.
                Value::text(if col.generated_stored_expr.is_some() {
                    "s"
                } else {
                    ""
                }),
                Value::Bool(false), // attisdropped
                Value::Bool(true),  // attislocal — true (not inherited)
                Value::Int(0),      // attinhcount
                Value::BigInt(0),   // attcollation — 0 (default)
                crate::acl::render_acl_list(&col.acl).map_or(Value::Null, Value::text),
            ]));
        }
        attrelid = attrelid.saturating_add(1);
    }
    // v7.39 (round 338, V64) — a view's columns. pg_attribute was
    // table-only, so `pg_attribute WHERE attrelid = '<view>'::regclass`
    // — the join psql \d and every reflection tool run to learn a view's
    // shape — came back empty even though information_schema.columns
    // (round 268) already knew the answer. Same resolver, so the two agree.
    for vname in cat.views_all().keys() {
        let Some(vname) = cat.listed_name(vname) else {
            continue;
        };
        let Some(view_oid) = relation_oid(cat, vname) else {
            continue;
        };
        for (i, col) in crate::describe::describe_view_columns(cat, vname)
            .iter()
            .enumerate()
        {
            #[allow(clippy::cast_possible_wrap)]
            let attnum = (i + 1) as i16;
            let typlen: i16 = pg_type_len(col.ty);
            rows.push(Row::new(alloc::vec![
                Value::BigInt(view_oid),
                Value::text(col.name.clone()),
                Value::BigInt(pg_type_oid(col.ty)),
                Value::Int(-1),
                Value::SmallInt(typlen),
                Value::SmallInt(attnum),
                Value::Int(0),
                Value::Int(-1),
                Value::Bool(typlen > 0 && typlen <= 8),
                Value::text(if typlen > 0 { "p" } else { "x" }),
                Value::text(match typlen {
                    1 => "c",
                    2 => "s",
                    4 => "i",
                    _ => "d",
                }),
                // A view column is nullable regardless of the base
                // column's constraint — an outer join can null it.
                Value::Bool(false),
                Value::Bool(false), // atthasdef
                Value::text(""),    // attidentity
                Value::text(""),    // attgenerated
                Value::Bool(false), // attisdropped
                Value::Bool(true),  // attislocal
                Value::Int(0),
                Value::BigInt(0),
                Value::Null, // attacl
            ]));
        }
    }
    (schema, rows)
}

/// v7.39 (round 338) — PG's typlen for a type: the fixed width in bytes,
/// -1 for a var-length one. Shared by the table and view attribute rows.
const fn pg_type_len(ty: DataType) -> i16 {
    match ty {
        DataType::Bool => 1,
        DataType::SmallInt => 2,
        DataType::Int | DataType::Date => 4,
        DataType::BigInt | DataType::Float | DataType::Timestamp | DataType::Timestamptz => 8,
        _ => -1,
    }
}

/// PG type OID lookup for the SPG DataType set. Used by
/// `synth_pg_attribute`'s `atttypid` column.
fn pg_type_oid(ty: DataType) -> i64 {
    match ty {
        DataType::Bool => 16,
        DataType::Bytes => 17,
        // v7.39 (round 291) — PG's identifier type has its own OID; the
        // catch-all mapped it to text (25) and `format_type` then had
        // no way back to the name.
        DataType::Name => 19,
        DataType::SmallInt => 21,
        DataType::Int => 23,
        DataType::BigInt => 20,
        DataType::Text | DataType::Varchar(_) | DataType::Char(_) => 25,
        DataType::Float => 701,
        DataType::Numeric { .. } => 1700,
        DataType::Date => 1082,
        DataType::Time => 1083,
        DataType::TimeTz => 1266,
        DataType::Timestamp => 1114,
        DataType::Timestamptz => 1184,
        DataType::Interval => 1186,
        DataType::Uuid => 2950,
        DataType::Json => 114,
        DataType::Jsonb => 3802,
        DataType::TextArray => 1009,
        DataType::IntArray => 1007,
        DataType::BigIntArray => 1016,
        DataType::SmallIntArray => 1005,
        DataType::FloatArray => 1022,
        DataType::BoolArray => 1000,
        DataType::DateArray => 1182,
        DataType::TimestampArray => 1115,
        DataType::TimestamptzArray => 1185,
        DataType::UuidArray => 2951,
        DataType::BytesArray => 1001,
        DataType::NumericArray => 1231,
        DataType::JsonArray => 199,
        // 2-D arrays use the same element-array OID as 1-D —
        // PG distinguishes dimensions via attndims, not OID.
        DataType::IntArray2D => 1007,
        DataType::BigIntArray2D => 1016,
        DataType::TextArray2D => 1009,
        _ => 0,
    }
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
    // v7.37.24 (24.7) — widened from 8 to 16 PG-canonical columns.
    // ORMs / monitoring tools query typbyval / typispreferred /
    // typdelim / typisdefined to decide encoding strategies; the
    // shape matches PG exactly so introspection round-trips
    // succeed.
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("typname", DataType::Text, false),
        ColumnSchema::new("typnamespace", DataType::BigInt, false),
        ColumnSchema::new("typowner", DataType::BigInt, false),
        ColumnSchema::new("typlen", DataType::SmallInt, false),
        ColumnSchema::new("typbyval", DataType::Bool, false),
        ColumnSchema::new("typtype", DataType::Text, false),
        ColumnSchema::new("typcategory", DataType::Text, false),
        ColumnSchema::new("typispreferred", DataType::Bool, false),
        ColumnSchema::new("typisdefined", DataType::Bool, false),
        ColumnSchema::new("typdelim", DataType::Text, false),
        ColumnSchema::new("typrelid", DataType::BigInt, false),
        ColumnSchema::new("typsubscript", DataType::Text, false),
        ColumnSchema::new("typelem", DataType::BigInt, false),
        ColumnSchema::new("typarray", DataType::BigInt, false),
        ColumnSchema::new("typalign", DataType::Text, false),
        ColumnSchema::new("typstorage", DataType::Text, false),
        ColumnSchema::new("typnotnull", DataType::Bool, false),
        ColumnSchema::new("typbasetype", DataType::BigInt, false),
        ColumnSchema::new("typtypmod", DataType::Int, false),
        ColumnSchema::new("typndims", DataType::Int, false),
        ColumnSchema::new("typcollation", DataType::BigInt, false),
    ];
    // (oid, name, len, type, cat, elem, array_oid). PG OID
    // numbers come straight from `pg_type.dat`.
    let scalars: &[(i64, &str, i16, &str, &str, i64, i64)] = &[
        // bool
        (16, "bool", 1, "b", "B", 0, 1000),
        (17, "bytea", -1, "b", "U", 0, 1001),
        // v7.39 (pg_type reconcile) — "char" is category Z (internal);
        // name's element type is "char" (oid 18), per PG18.
        (18, "char", 1, "b", "Z", 0, 1002),
        (19, "name", 64, "b", "S", 18, 1003),
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
        // v7.39 (pg_type reconcile) — these two were transposed; PG18:
        // 3908 = tsrange, 3910 = tstzrange (the wire layer already
        // encoded them correctly, so only the catalog disagreed).
        (3908, "tsrange", -1, "r", "R", 0, 3909),
        (3910, "tstzrange", -1, "r", "R", 0, 3911),
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
    // Build a row from PG's type-attribute conventions:
    //   typbyval        — fixed-width ∈ {1,2,4,8} (PG SQL_pass-by-value)
    //   typdelim        — ',' for all built-ins
    //   typalign        — 'c'/'s'/'i'/'d' from typlen
    //   typstorage      — 'p' for fixed-width, 'x' (extended) for varlena
    //   typispreferred  — true for canonical "preferred" type in category
    //     (text in 'S', int4 in 'N', timestamptz in 'D' — same as
    //     PG's typcategory preferred-conversion target)
    let preferred_oids: &[i64] = &[16, 25, 23, 1184, 1700];
    let build_row = |oid: i64,
                     name: &str,
                     len: i16,
                     ty: &str,
                     cat: &str,
                     elem: i64,
                     arr: i64,
                     subscript: &str|
     -> Row<'static> {
        let typbyval = len > 0 && len <= 8;
        let typalign = match len {
            1 => "c",
            2 => "s",
            4 => "i",
            _ => "d",
        };
        let typstorage = if len > 0 { "p" } else { "x" };
        let typispreferred = preferred_oids.contains(&oid);
        Row::new(alloc::vec![
            Value::BigInt(oid),
            Value::text::<String>(name.into()),
            Value::BigInt(2200), // typnamespace
            Value::BigInt(10),   // typowner (postgres superuser OID)
            Value::SmallInt(len),
            Value::Bool(typbyval),
            Value::text::<String>(ty.into()),
            Value::text::<String>(cat.into()),
            Value::Bool(typispreferred),
            Value::Bool(true),                       // typisdefined
            Value::text::<String>(",".into()),       // typdelim
            Value::BigInt(0),                        // typrelid (composite-type table OID)
            Value::text::<String>(subscript.into()), // typsubscript
            Value::BigInt(elem),
            Value::BigInt(arr),
            Value::text::<String>(typalign.into()),
            Value::text::<String>(typstorage.into()),
            Value::Bool(false), // typnotnull — base types are nullable
            Value::BigInt(0),   // typbasetype (DOMAIN base; 0 for base types)
            Value::Int(-1),     // typtypmod
            Value::Int(0),      // typndims
            Value::BigInt(0),   // typcollation — 0 (default)
        ])
    };
    for &(oid, name, len, ty, cat, elem, arr) in scalars {
        rows.push(build_row(oid, name, len, ty, cat, elem, arr, "-"));
    }
    for &(oid, name, elem) in arrays {
        rows.push(build_row(
            oid,
            name,
            -1,
            "b",
            "A",
            elem,
            0,
            "array_subscript_handler",
        ));
    }
    // v7.39 (round 330, V48) — the four domains `information_schema` is
    // built out of. PG has them in pg_type with `typtype = 'd'` and a real
    // `typbasetype` (measured: sql_identifier over name, character_data
    // and yes_or_no over character varying, cardinal_number over integer);
    // SPG reported nothing at all, so a client resolving the type behind
    // an information_schema column found no such type.
    for (full, base) in INFORMATION_SCHEMA_DOMAINS {
        let bare = full.rsplit('.').next().unwrap_or(full);
        let (base_oid, len): (i64, i16) = match base {
            DataType::Name => (19, 64),
            DataType::Int => (23, 4),
            _ => (1043, -1),
        };
        let mut row = build_row(
            INFORMATION_SCHEMA_DOMAIN_OID_BASE + base_oid,
            bare,
            len,
            "d",
            "S",
            0,
            0,
            "-",
        );
        // typnamespace → information_schema, typbasetype → the base type.
        row.values[2] = Value::BigInt(13000);
        if let Some(slot) = schema.iter().position(|c| c.name == "typbasetype") {
            row.values[slot] = Value::BigInt(base_oid);
        }
        rows.push(row);
    }
    (schema, rows)
}

/// v7.39 (round 330, V48) — OID base for the synthesised
/// information_schema domains. Kept clear of PG's built-in type OIDs.
pub(crate) const INFORMATION_SCHEMA_DOMAIN_OID_BASE: i64 = 13_500;

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

pub(crate) fn synth_pg_proc(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    // v7.37.24 (24.6) — widened from 6 to 20 PG-canonical columns
    // covering the function metadata that ORMs (Diesel, sea-orm)
    // and pgAdmin's function browser query: prolang for language
    // dispatch, prosrc as the body excerpt, proretset for SETOF,
    // proisstrict for NULL-handling, provolatile / proparallel /
    // procost for planner annotations.
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("proname", DataType::Text, false),
        ColumnSchema::new("pronamespace", DataType::BigInt, false),
        ColumnSchema::new("proowner", DataType::BigInt, false),
        ColumnSchema::new("prolang", DataType::BigInt, false),
        ColumnSchema::new("procost", DataType::Float, false),
        ColumnSchema::new("prorows", DataType::Float, false),
        ColumnSchema::new("provariadic", DataType::BigInt, false),
        ColumnSchema::new("prokind", DataType::Text, false),
        ColumnSchema::new("prosecdef", DataType::Bool, false),
        ColumnSchema::new("proleakproof", DataType::Bool, false),
        ColumnSchema::new("proisstrict", DataType::Bool, false),
        ColumnSchema::new("proretset", DataType::Bool, false),
        ColumnSchema::new("provolatile", DataType::Text, false),
        ColumnSchema::new("proparallel", DataType::Text, false),
        ColumnSchema::new("pronargs", DataType::SmallInt, false),
        ColumnSchema::new("pronargdefaults", DataType::SmallInt, false),
        ColumnSchema::new("prorettype", DataType::BigInt, false),
        ColumnSchema::new("proargtypes", DataType::Text, false),
        ColumnSchema::new("prosrc", DataType::Text, false),
        // v7.39 (read01 round 61) — the function ACL. NULL means PG's default:
        // PUBLIC may EXECUTE.
        ColumnSchema::new("proacl", DataType::Text, true),
    ];
    let funcs: &[(i64, &str, &str, i32, i64)] = PG_PROC_FUNCS;
    let mut rows: Vec<Row<'static>> = Vec::with_capacity(funcs.len());
    // PG conventions for SPG-internal builtins:
    // - prolang = 12 (internal: built-in C function)
    // - procost = 1 (default cost; planner uses for tie-breaking)
    // - prorows = 0 for scalar functions, 1000 for set-returning
    // - provariadic = 0 (no built-in below is VARIADIC by signature)
    // - provolatile: 'i' immutable, 's' stable, 'v' volatile
    //   (now/random/current_timestamp = 'v'; date_trunc = 'i'; etc.)
    // - proparallel: 's' safe (default for pure functions),
    //   'r' restricted, 'u' unsafe
    // - proisstrict: most builtins are strict (NULL-on-NULL)
    // - prosrc: PG's internal entry-point name; SPG uses the
    //   function name itself as the body excerpt
    let volatile_names: &[&str] = &[
        "now",
        "random",
        "gen_random_uuid",
        "current_database",
        "current_user",
        "session_user",
        "current_schema",
    ];
    for &(oid, name, kind, nargs, rettype) in funcs {
        let provolatile: &str = if volatile_names.contains(&name) {
            "v"
        } else {
            "i"
        };
        let prorows: f64 = match kind {
            "a" | "w" => 1000.0,
            _ => 0.0,
        };
        // proargtypes — PG int2vector encoding; we don't have the
        // per-arg type list in the table, so synthesise N placeholder
        // zeros. ORMs that need the real list will fall back to the
        // pg_proc.dat columns of pg_proc-loaded extensions.
        let arg_count = if nargs < 0 { 0 } else { nargs };
        let mut argtypes = alloc::string::String::new();
        for i in 0..arg_count {
            if i > 0 {
                argtypes.push(' ');
            }
            argtypes.push('0');
        }
        rows.push(Row::new(alloc::vec![
            Value::BigInt(oid),
            Value::text::<String>(name.into()),
            Value::BigInt(11), // pronamespace = pg_catalog
            Value::BigInt(10), // proowner
            Value::BigInt(12), // prolang = internal
            Value::Float(1.0), // procost
            Value::Float(prorows),
            Value::BigInt(0), // provariadic
            Value::text::<String>(kind.into()),
            Value::Bool(false),       // prosecdef
            Value::Bool(false),       // proleakproof
            Value::Bool(true),        // proisstrict
            Value::Bool(kind == "w"), // proretset — window funcs return per-row sets
            Value::text::<String>(provolatile.into()),
            Value::text::<String>("s".into()), // proparallel = safe
            Value::SmallInt(i16::try_from(nargs.max(0)).unwrap_or(i16::MAX)),
            Value::SmallInt(0), // pronargdefaults
            Value::BigInt(rettype),
            Value::text(argtypes),
            Value::text::<String>(name.into()), // prosrc
            Value::Null,                        // proacl — a builtin's is never set
        ]));
    }
    // v7.39 (read01 round 61) — and one row per USER-DEFINED function. They were
    // missing entirely, so `SELECT proacl FROM pg_proc WHERE proname = 'f1'` —
    // the canonical way to read a function's privileges — came back empty.
    let mut user_oid: i64 = OID_FUNC_BASE;
    // v7.39 (read01 round 62) — the map is keyed by SIGNATURE now, so `proname`
    // comes off the definition, not the key: two overloads are two rows sharing
    // one name, exactly as in PG.
    for def in cat.functions().values() {
        user_oid += 1;
        let nargs = crate::acl::function_arg_count(&def.args_repr);
        rows.push(Row::new(alloc::vec![
            Value::BigInt(user_oid),
            Value::text(def.name.clone()),
            Value::BigInt(2200), // pronamespace — public
            Value::BigInt(10),   // proowner
            // prolang: 14 = sql, 13 = plpgsql (PG's oids).
            Value::BigInt(if def.language.eq_ignore_ascii_case("plpgsql") {
                13
            } else {
                14
            }),
            // v7.39 (round 322, V46) — the DECLARED attributes, not a row
            // of PG defaults: `CREATE FUNCTION … IMMUTABLE STRICT` was a
            // parse error until this round, so there was nothing else to
            // report; now there is.
            Value::Float(def.cost.unwrap_or(100.0)),
            Value::Float(def.rows.unwrap_or(0.0)),
            Value::BigInt(0),
            Value::text("f"), // prokind — a normal function
            Value::Bool(def.security_definer),
            Value::Bool(def.leakproof),
            Value::Bool(def.strict),
            Value::Bool(false),
            Value::text(alloc::string::String::from(
                core::str::from_utf8(&[def.volatility]).unwrap_or("v"),
            )), // provolatile
            Value::text(alloc::string::String::from(
                core::str::from_utf8(&[def.parallel]).unwrap_or("u"),
            )), // proparallel
            Value::SmallInt(i16::try_from(nargs).unwrap_or(i16::MAX)),
            Value::SmallInt(0),
            Value::BigInt(0),
            Value::text(alloc::string::String::new()),
            Value::text(def.body.clone()), // prosrc — the real body
            crate::acl::render_acl_list(&def.acl).map_or(Value::Null, Value::text),
        ]));
    }
    (schema, rows)
}

/// (oid, name, kind, nargs, rettype). OIDs taken from PG's pg_proc.dat
/// for the common subset. v7.39 (read01 regproc.c) — module-level so the
/// regproc/regprocedure casts resolve names against the same table
/// pg_proc synthesises.
pub(crate) const PG_PROC_FUNCS: &[(i64, &str, &str, i32, i64)] = &[
    // Scalar functions.
    // PG ships eight length() overloads; mirror the full set so
    // catalog joins see the same rows (1317 text, 1318 bpchar,
    // 1530/1531 lseg/path -> float8, 1681 bit, 1713 length(bytea,
    // name) [2 args], 2010 bytea, 3711 tsvector).
    (1317, "length", "f", 1, 23),
    (1318, "length", "f", 1, 23),
    (1530, "length", "f", 1, 701),
    (1531, "length", "f", 1, 701),
    (1681, "length", "f", 1, 23),
    (1713, "length", "f", 2, 23),
    (2010, "length", "f", 1, 23),
    (3711, "length", "f", 1, 23),
    // v7.39 (pg_proc reconcile) — every row below carries PG18's
    // real (oid, prokind, pronargs, prorettype); the old table was
    // full of invented / transposed oids (found when the corpus
    // campaign hit two wrong rows and a full differential audit
    // showed the drift was systemic). VARIADIC functions have
    // pronargs = 1 (or 2 with a leading fixed arg), like PG.
    (870, "lower", "f", 1, 25),
    (871, "upper", "f", 1, 25),
    // v7.39 (read01 regproc.c) — the range overloads make
    // lower/upper ambiguous for ::regproc, as in PG.
    (3848, "lower", "f", 1, 2283),
    (3849, "upper", "f", 1, 2283),
    (936, "substring", "f", 3, 25),
    (937, "substring", "f", 2, 25),
    (885, "btrim", "f", 1, 25),
    (884, "btrim", "f", 2, 25),
    (881, "ltrim", "f", 1, 25),
    (875, "ltrim", "f", 2, 25),
    (882, "rtrim", "f", 1, 25),
    (876, "rtrim", "f", 2, 25),
    (1396, "abs", "f", 1, 20),
    (1397, "abs", "f", 1, 23),
    (1705, "abs", "f", 1, 1700),
    (1342, "round", "f", 1, 701),
    (1708, "round", "f", 1, 1700),
    (1707, "round", "f", 2, 1700),
    (2308, "ceil", "f", 1, 701),
    (1711, "ceil", "f", 1, 1700),
    (2320, "ceiling", "f", 1, 701),
    (2167, "ceiling", "f", 1, 1700),
    (2309, "floor", "f", 1, 701),
    (1712, "floor", "f", 1, 1700),
    (1344, "sqrt", "f", 1, 701),
    (1730, "sqrt", "f", 1, 1700),
    (1341, "ln", "f", 1, 701),
    (1734, "ln", "f", 1, 1700),
    (1347, "exp", "f", 1, 701),
    (1732, "exp", "f", 1, 1700),
    (1368, "power", "f", 2, 701),
    (2169, "power", "f", 2, 1700),
    (1598, "random", "f", 0, 701),
    // Date / time. (current_date / current_timestamp /
    // current_time are parser keywords in PG, not pg_proc rows.)
    (1299, "now", "f", 0, 1184),
    (2020, "date_trunc", "f", 2, 1114),
    (2021, "date_part", "f", 2, 701),
    (2059, "age", "f", 1, 1186),
    (2058, "age", "f", 2, 1186),
    (2049, "to_char", "f", 2, 25),
    (1772, "to_char", "f", 2, 25),
    // Session / introspection.
    (861, "current_database", "f", 0, 19),
    (745, "current_user", "f", 0, 19),
    (746, "session_user", "f", 0, 19),
    (1402, "current_schema", "f", 0, 19),
    // String concat / format.
    (3058, "concat", "f", 1, 25),
    (3059, "concat_ws", "f", 2, 25),
    (3539, "format", "f", 2, 25),
    (3540, "format", "f", 1, 25),
    // Type introspection.
    (1619, "pg_typeof", "f", 1, 2206),
    // JSON.
    (3200, "json_build_object", "f", 1, 114),
    (3273, "jsonb_build_object", "f", 1, 3802),
    (3198, "json_build_array", "f", 1, 114),
    (3271, "jsonb_build_array", "f", 1, 3802),
    // UUID.
    (3432, "gen_random_uuid", "f", 0, 2950),
    // Aggregates.
    // PG: 2147 = count(any) [1 arg], 2803 = count(*) [0 args].
    (2147, "count", "a", 1, 20),
    (2803, "count", "a", 0, 20),
    (2116, "max", "a", 1, 23),
    (2129, "max", "a", 1, 25),
    (2130, "max", "a", 1, 1700),
    (2132, "min", "a", 1, 23),
    (2145, "min", "a", 1, 25),
    (2146, "min", "a", 1, 1700),
    (2108, "sum", "a", 1, 20),
    (2114, "sum", "a", 1, 1700),
    (2100, "avg", "a", 1, 1700),
    (3538, "string_agg", "a", 2, 25),
    (2335, "array_agg", "a", 1, 2277),
    (2517, "bool_and", "a", 1, 16),
    (2518, "bool_or", "a", 1, 16),
    (2519, "every", "a", 1, 16),
    // Window functions.
    (3100, "row_number", "w", 0, 20),
    (3101, "rank", "w", 0, 20),
    (3102, "dense_rank", "w", 0, 20),
    (3103, "percent_rank", "w", 0, 701),
    (3104, "cume_dist", "w", 0, 701),
    (3106, "lag", "w", 1, 2283),
    (3107, "lag", "w", 2, 2283),
    (3109, "lead", "w", 1, 2283),
    (3110, "lead", "w", 2, 2283),
    (3112, "first_value", "w", 1, 2283),
    (3113, "last_value", "w", 1, 2283),
    (3114, "nth_value", "w", 2, 2283),
];

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
/// v7.37.17 — synthesise `information_schema.constraint_column_usage`.
/// PG semantics: PK/UNIQUE rows list the constrained columns on
/// their own table; FK rows list the columns of the REFERENCED
/// table. (CHECK column extraction needs expression analysis —
/// queued.) ORM relationship builders join this against
/// table_constraints/key_column_usage.
pub(crate) fn synth_info_constraint_column_usage(
    cat: &Catalog,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("table_catalog", DataType::Text, false),
        ColumnSchema::new("table_schema", DataType::Text, false),
        ColumnSchema::new("table_name", DataType::Text, false),
        ColumnSchema::new("column_name", DataType::Text, false),
        ColumnSchema::new("constraint_catalog", DataType::Text, false),
        ColumnSchema::new("constraint_schema", DataType::Text, false),
        ColumnSchema::new("constraint_name", DataType::Text, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    let mut push = |table: &str, column: String, conname: String| {
        rows.push(Row::new(alloc::vec![
            Value::text("spg"),
            Value::text("public"),
            Value::text(table.to_string()),
            Value::text(column),
            Value::text("spg"),
            Value::text("public"),
            Value::text(conname),
        ]));
    };
    for tname in cat.visible_table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        let cols = &t.schema().columns;
        let col_name_at = |pos: usize| -> String {
            cols.get(pos)
                .map_or_else(|| alloc::format!("col{pos}"), |c| c.name.clone())
        };
        for uc in t.schema().uniqueness_constraints.iter() {
            let conname = pg_unique_conname(t, uc, &tname);
            for &p in &uc.columns {
                push(&tname, col_name_at(p), conname.clone());
            }
        }
        for fk in t.schema().foreign_keys.iter() {
            let conname = fk
                .name
                .clone()
                .unwrap_or_else(|| pg_fk_conname(t, fk, &tname));
            // FK rows reference the PARENT table's columns.
            if let Some(parent) = cat.get(&fk.parent_table) {
                for &p in &fk.parent_columns {
                    let pname = parent
                        .schema()
                        .columns
                        .get(p)
                        .map_or_else(|| alloc::format!("col{p}"), |c| c.name.clone());
                    push(&fk.parent_table, pname, conname.clone());
                }
            }
        }
        // v7.39 (round 266) — CHECK rows, one per column the expression
        // mentions (PG explodes a multi-column CHECK across its
        // columns), and the NOT NULL pseudo-constraints. The column
        // extraction is the same one that names a CHECK, so the name
        // here and in check_constraints cannot drift apart.
        let check_names = pg_check_connames(t, &tname, &t.schema().checks);
        for (ci, chk) in t.schema().checks.iter().enumerate() {
            for col in referenced_columns(t, &chk.expr) {
                push(&tname, col, check_names[ci].clone());
            }
        }
        for col in cols.iter() {
            if col.nullable {
                continue;
            }
            push(
                &tname,
                col.name.clone(),
                alloc::format!("{tname}_{}_not_null", col.name),
            );
        }
    }
    (schema, rows)
}

/// v7.37.17 — synthesise `information_schema.triggers`.
/// pgAdmin's trigger panel and SQLAlchemy read this. PG explodes
/// one row per (trigger × event); SPG mirrors that.
pub(crate) fn synth_info_triggers(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("trigger_catalog", DataType::Text, false),
        ColumnSchema::new("trigger_schema", DataType::Text, false),
        ColumnSchema::new("trigger_name", DataType::Text, false),
        ColumnSchema::new("event_manipulation", DataType::Text, false),
        ColumnSchema::new("event_object_schema", DataType::Text, false),
        ColumnSchema::new("event_object_table", DataType::Text, false),
        ColumnSchema::new("action_statement", DataType::Text, false),
        ColumnSchema::new("action_orientation", DataType::Text, false),
        ColumnSchema::new("action_timing", DataType::Text, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for trg in cat.triggers() {
        for event in &trg.events {
            rows.push(Row::new(alloc::vec![
                Value::text("spg"),
                Value::text("public"),
                Value::text(trg.name.clone()),
                Value::text(event.clone()),
                Value::text("public"),
                Value::text(trg.table.clone()),
                Value::text(alloc::format!("EXECUTE FUNCTION {}()", trg.function)),
                Value::text(trg.for_each.clone()),
                Value::text(trg.timing.clone()),
            ]));
        }
    }
    (schema, rows)
}

/// v7.37.17 — synthesise `information_schema.check_constraints`.
/// One row per CHECK expression, named `{table}_check{i}` (the
/// same synthetic convention pg_constraint's CHECK rows use).
pub(crate) fn synth_info_check_constraints(
    cat: &Catalog,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("constraint_catalog", DataType::Text, false),
        ColumnSchema::new("constraint_schema", DataType::Text, false),
        ColumnSchema::new("constraint_name", DataType::Text, false),
        ColumnSchema::new("check_clause", DataType::Text, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for tname in cat.visible_table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        // Same PG-canonical `{table}_{col}_check` naming pg_constraint
        // and pg_get_constraintdef use, so the three agree.
        let check_names = pg_check_connames(t, &tname, &t.schema().checks);
        for (ci, clause) in t.schema().checks.iter().enumerate() {
            rows.push(Row::new(alloc::vec![
                Value::text("spg"),
                Value::text("public"),
                Value::text(check_names[ci].clone()),
                Value::text(clause.expr.clone()),
            ]));
        }
        // v7.39 (round 266) — PG 18 models each NOT NULL column as a
        // real CHECK constraint, so it surfaces here too with the
        // clause it would have been written as. table_constraints
        // already listed these rows; check_constraints did not, which
        // left the two views disagreeing about the same constraint.
        for col in t.schema().columns.iter() {
            if col.nullable {
                continue;
            }
            rows.push(Row::new(alloc::vec![
                Value::text("spg"),
                Value::text("public"),
                Value::text(alloc::format!("{tname}_{}_not_null", col.name)),
                Value::text(alloc::format!("{} IS NOT NULL", col.name)),
            ]));
        }
    }
    (schema, rows)
}

/// v7.37.17 — synthesise `information_schema.sequences`.
/// SQLAlchemy's sequence reflection and pgAdmin's sequence browser
/// read this; one row per catalog sequence with its declared
/// bounds.
pub(crate) fn synth_info_sequences(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("sequence_catalog", DataType::Text, false),
        ColumnSchema::new("sequence_schema", DataType::Text, false),
        ColumnSchema::new("sequence_name", DataType::Text, false),
        ColumnSchema::new("data_type", DataType::Text, false),
        ColumnSchema::new("start_value", DataType::BigInt, false),
        ColumnSchema::new("minimum_value", DataType::BigInt, false),
        ColumnSchema::new("maximum_value", DataType::BigInt, false),
        ColumnSchema::new("increment", DataType::BigInt, false),
        ColumnSchema::new("cycle_option", DataType::Text, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for (name, def) in cat.sequences_all() {
        let Some(name) = cat.listed_name(name) else {
            continue;
        };
        let dt = match def.data_type {
            spg_storage::SequenceDataType::SmallInt => "smallint",
            spg_storage::SequenceDataType::Int => "integer",
            spg_storage::SequenceDataType::BigInt => "bigint",
        };
        rows.push(Row::new(alloc::vec![
            Value::text("spg"),
            Value::text("public"),
            Value::text(name.to_string()),
            Value::text::<&str>(dt),
            Value::BigInt(def.start),
            Value::BigInt(def.min_value),
            Value::BigInt(def.max_value),
            Value::BigInt(def.increment),
            Value::text::<&str>(if def.cycle { "YES" } else { "NO" }),
        ]));
    }
    (schema, rows)
}

pub(crate) fn synth_info_key_column_usage(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("constraint_name", DataType::Text, false),
        ColumnSchema::new("table_name", DataType::Text, false),
        ColumnSchema::new("column_name", DataType::Text, false),
        ColumnSchema::new("ordinal_position", DataType::Int, false),
        ColumnSchema::new("referenced_table_name", DataType::Text, false),
        ColumnSchema::new("referenced_column_name", DataType::Text, false),
        // v7.39 (round 266) — where this column sits in the parent key
        // the FK points at. NULL on PK/UNIQUE rows, which is how a
        // reflection tool tells an FK row apart from a key row.
        ColumnSchema::new("position_in_unique_constraint", DataType::Int, true),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for tname in cat.visible_table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        let cols = &t.schema().columns;
        let col_name_at = |pos: usize| -> String {
            cols.get(pos)
                .map_or_else(|| alloc::format!("col{pos}"), |c| c.name.clone())
        };
        // FKs.
        for fk in t.schema().foreign_keys.iter() {
            let conname = fk
                .name
                .clone()
                .unwrap_or_else(|| pg_fk_conname(t, fk, &tname));
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
                // Where the referenced column sits inside the PARENT's
                // key, which is not necessarily where it sits in this
                // FK's own column list when the two are written in a
                // different order.
                let in_unique = cat
                    .get(&fk.parent_table)
                    .and_then(|pt| {
                        pt.schema()
                            .uniqueness_constraints
                            .iter()
                            .find(|uc| {
                                uc.columns.len() == fk.parent_columns.len()
                                    && fk.parent_columns.iter().all(|pc| uc.columns.contains(pc))
                            })
                            .and_then(|uc| uc.columns.iter().position(|&c| c == parent))
                    })
                    .unwrap_or(i);
                #[allow(clippy::cast_possible_wrap)]
                let in_unique = (in_unique + 1) as i32;
                rows.push(Row::new(alloc::vec![
                    Value::text(conname.clone()),
                    Value::text(tname.clone()),
                    Value::text(col_name_at(local)),
                    Value::Int(ordinal),
                    Value::text(fk.parent_table.clone()),
                    Value::text(parent_name),
                    Value::Int(in_unique),
                ]));
            }
        }
        // PK / composite UC entries.
        for uc in t.schema().uniqueness_constraints.iter() {
            let conname = pg_unique_conname(t, uc, &tname);
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
                    Value::Null,
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
        // The PK/UNIQUE constraint on the parent that the FK targets —
        // JDBC getImportedKeys / ORM reflection reads this to link the
        // FK back to the referenced key.
        ColumnSchema::new("unique_constraint_name", DataType::Text, true),
        ColumnSchema::new("update_rule", DataType::Text, false),
        ColumnSchema::new("delete_rule", DataType::Text, false),
        // v7.39 (round 266) — the four SQL-standard qualifiers plus
        // match_option. The view was built from the MySQL-flavoured
        // column set (table_name / referenced_table_name above), so the
        // PG names a reflection tool actually selects were absent.
        // match_option reports the FK's stored MATCH type, which is the
        // one the enforcement path in constraints.rs honours — PG spells
        // the default `NONE`, not `SIMPLE`.
        ColumnSchema::new("constraint_catalog", DataType::Text, false),
        ColumnSchema::new("constraint_schema", DataType::Text, false),
        ColumnSchema::new("unique_constraint_catalog", DataType::Text, true),
        ColumnSchema::new("unique_constraint_schema", DataType::Text, true),
        ColumnSchema::new("match_option", DataType::Text, false),
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
    for tname in cat.visible_table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        for fk in t.schema().foreign_keys.iter() {
            let conname = fk
                .name
                .clone()
                .unwrap_or_else(|| pg_fk_conname(t, fk, &tname));
            // The parent-side UNIQUE / PK constraint covering exactly the
            // referenced columns.
            let unique_name: Value<'static> = cat
                .get(&fk.parent_table)
                .and_then(|pt| {
                    pt.schema()
                        .uniqueness_constraints
                        .iter()
                        .find(|uc| {
                            uc.columns.len() == fk.parent_columns.len()
                                && fk.parent_columns.iter().all(|pc| uc.columns.contains(pc))
                        })
                        .map(|uc| pg_unique_conname(pt, uc, &fk.parent_table))
                })
                .map_or(Value::Null, Value::text);
            // The parent-side qualifiers exist only when the parent key
            // was actually located, so they track unique_constraint_name.
            let has_unique = !matches!(unique_name, Value::Null);
            let qualifier = |text: &'static str| {
                if has_unique {
                    Value::text(text)
                } else {
                    Value::Null
                }
            };
            rows.push(Row::new(alloc::vec![
                Value::text(conname),
                Value::text(tname.clone()),
                Value::text(fk.parent_table.clone()),
                unique_name,
                Value::text::<String>(rule_name(fk.on_update).into()),
                Value::text::<String>(rule_name(fk.on_delete).into()),
                Value::text("spg"),
                Value::text("public"),
                qualifier("spg"),
                qualifier("public"),
                Value::text::<&str>(match fk.match_type {
                    spg_storage::MatchType::Simple => "NONE",
                    spg_storage::MatchType::Full => "FULL",
                }),
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
    for tname in cat.visible_table_names() {
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
/// v7.37 U11 — synthesise `pg_catalog.pg_sequence`. One row per CREATE
/// SEQUENCE, exposing PG's seqstart/seqincrement/seqmax/seqmin/seqcache/
/// seqcycle columns from the catalog SequenceDef. Previously absent, so
/// psql `\d <seq>` and ORMs that read pg_sequence saw nothing.
pub(crate) fn synth_pg_sequence(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("seqrelid", DataType::BigInt, false),
        ColumnSchema::new("seqtypid", DataType::BigInt, false),
        ColumnSchema::new("seqstart", DataType::BigInt, false),
        ColumnSchema::new("seqincrement", DataType::BigInt, false),
        ColumnSchema::new("seqmax", DataType::BigInt, false),
        ColumnSchema::new("seqmin", DataType::BigInt, false),
        ColumnSchema::new("seqcache", DataType::BigInt, false),
        ColumnSchema::new("seqcycle", DataType::Bool, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    // v7.39 (round 338, V64) — seqrelid IS the sequence's pg_class oid, so
    // it reads from the shared allocator. It used to number sequences from
    // 32768 while pg_class numbered them from 300_000, which broke PG's
    // canonical `pg_class JOIN pg_sequence ON oid = seqrelid` outright —
    // and 32768 was the view band, so the two kinds collided besides.
    for (name, def) in cat.sequences_all() {
        let Some(name) = cat.listed_name(name) else {
            continue;
        };
        let Some(seq_oid) = relation_oid(cat, name) else {
            continue;
        };
        rows.push(Row::new(alloc::vec![
            Value::BigInt(seq_oid),
            Value::BigInt(20), // seqtypid — bigint (OID 20)
            Value::BigInt(def.start),
            Value::BigInt(def.increment),
            Value::BigInt(def.max_value),
            Value::BigInt(def.min_value),
            Value::BigInt(def.cache),
            Value::Bool(def.cycle),
        ]));
    }
    (schema, rows)
}

pub(crate) fn synth_pg_constraint(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    // v7.37.24 (24.8b-3) — widened from 6 to 17 PG-canonical
    // columns. Key change: conrelid + confrelid are now BigInt
    // OIDs (joinable with pg_class.oid). Tools depending on
    // `SELECT … FROM pg_constraint c JOIN pg_class p ON
    // c.conrelid = p.oid` (ORM relationship-graph builders) now
    // resolve rows correctly.
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("conname", DataType::Text, false),
        ColumnSchema::new("connamespace", DataType::BigInt, false),
        ColumnSchema::new("contype", DataType::Text, false),
        ColumnSchema::new("condeferrable", DataType::Bool, false),
        ColumnSchema::new("condeferred", DataType::Bool, false),
        ColumnSchema::new("convalidated", DataType::Bool, false),
        ColumnSchema::new("conrelid", DataType::BigInt, false),
        ColumnSchema::new("contypid", DataType::BigInt, false),
        ColumnSchema::new("conindid", DataType::BigInt, false),
        ColumnSchema::new("conparentid", DataType::BigInt, false),
        ColumnSchema::new("confrelid", DataType::BigInt, false),
        ColumnSchema::new("confupdtype", DataType::Text, false),
        ColumnSchema::new("confdeltype", DataType::Text, false),
        ColumnSchema::new("confmatchtype", DataType::Text, false),
        ColumnSchema::new("conislocal", DataType::Bool, false),
        ColumnSchema::new("coninhcount", DataType::Int, false),
        ColumnSchema::new("connoinherit", DataType::Bool, false),
        ColumnSchema::new("conkey", DataType::Text, false),
        ColumnSchema::new("confkey", DataType::Text, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    // Build the same name → oid map pg_class uses (start at 16384).
    let names = cat.visible_table_names();
    let mut by_table: alloc::collections::BTreeMap<String, i64> =
        alloc::collections::BTreeMap::new();
    let mut next_oid: i64 = 16384;
    for tname in &names {
        by_table.insert(tname.clone(), next_oid);
        next_oid = next_oid.saturating_add(1);
    }
    // Constraint OIDs live in their own monotonically-increasing
    // band (PG starts above 65536 for catalog-touch artefacts).
    let mut con_oid: i64 = 65536;
    let mut next_con_oid = || -> i64 {
        let v = con_oid;
        con_oid += 1;
        v
    };
    for tname in &names {
        let Some(t) = cat.get(tname) else { continue };
        let conrelid = *by_table.get(tname).unwrap_or(&0);
        let cols = &t.schema().columns;
        let col_name_at = |pos: usize| -> String {
            cols.get(pos)
                .map_or_else(|| alloc::format!("col{pos}"), |c| c.name.clone())
        };
        // PG's `conkey` / `confkey` are smallint[] — render the array
        // literal form psql shows (`{1,2}`, 1-based attnums).
        let conkey_vec = |positions: &[usize]| -> String {
            let mut s = String::from("{");
            for (i, p) in positions.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str(&alloc::format!("{}", p + 1));
            }
            s.push('}');
            s
        };
        // Uniqueness constraints.
        for uc in t.schema().uniqueness_constraints.iter() {
            let kind = if uc.is_primary_key { "p" } else { "u" };
            let conname = pg_unique_conname(t, uc, tname);
            let conkey_display = conkey_vec(&uc.columns);
            rows.push(Row::new(alloc::vec![
                Value::BigInt(next_con_oid()),
                Value::text(conname),
                Value::BigInt(2200),
                Value::text::<String>(kind.into()),
                Value::Bool(false), // condeferrable
                Value::Bool(false), // condeferred
                Value::Bool(true),  // convalidated
                Value::BigInt(conrelid),
                Value::BigInt(0),  // contypid
                Value::BigInt(0),  // conindid — pending UC↔index plumb-through
                Value::BigInt(0),  // conparentid
                Value::BigInt(0),  // confrelid (not an FK)
                Value::text(" "),  // confupdtype
                Value::text(" "),  // confdeltype
                Value::text(" "),  // confmatchtype
                Value::Bool(true), // conislocal
                Value::Int(0),     // coninhcount
                Value::Bool(true), // connoinherit
                Value::text(conkey_display),
                Value::text(String::new()),
            ]));
        }
        // Single-column unique indices that don't have a UC entry.
        // v7.39 (read01 ruleutils.c) — a PARTIAL unique index is never a
        // constraint in PG (constraints cannot carry predicates).
        for idx in t.indices() {
            if !idx.is_unique || idx.partial_predicate.is_some() {
                continue;
            }
            let already = t
                .schema()
                .uniqueness_constraints
                .iter()
                .any(|uc| uc.columns.len() == 1 && uc.columns[0] == idx.column_position);
            if already {
                continue;
            }
            let is_primary = idx.name.ends_with("_pkey");
            let kind = if is_primary { "p" } else { "u" };
            let positions = alloc::vec![idx.column_position];
            let conkey_display = conkey_vec(&positions);
            rows.push(Row::new(alloc::vec![
                Value::BigInt(next_con_oid()),
                Value::text(idx.name.clone()),
                Value::BigInt(2200),
                Value::text::<String>(kind.into()),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(true),
                Value::BigInt(conrelid),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::text(" "),
                Value::text(" "),
                Value::text(" "),
                Value::Bool(true),
                Value::Int(0),
                Value::Bool(true),
                Value::text(conkey_display),
                Value::text(String::new()),
            ]));
        }
        // Foreign keys.
        for fk in t.schema().foreign_keys.iter() {
            let conname = fk
                .name
                .clone()
                .unwrap_or_else(|| pg_fk_conname(t, fk, tname));
            let confrelid = by_table.get(&fk.parent_table).copied().unwrap_or(0);
            let conkey = conkey_vec(&fk.local_columns);
            let confkey = conkey_vec(&fk.parent_columns);
            // confupdtype / confdeltype: 'a' no action, 'r' restrict,
            // 'c' cascade, 'n' set null, 'd' set default. SPG's
            // ForeignKey action enum already mirrors this.
            let upd_action = fk_action_char(fk.on_update);
            let del_action = fk_action_char(fk.on_delete);
            rows.push(Row::new(alloc::vec![
                Value::BigInt(next_con_oid()),
                Value::text(conname),
                Value::BigInt(2200),
                Value::text("f"),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(true),
                Value::BigInt(conrelid),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::BigInt(confrelid),
                Value::text::<String>(upd_action.into()),
                Value::text::<String>(del_action.into()),
                Value::text("s"), // confmatchtype: 's' SIMPLE (default)
                Value::Bool(true),
                Value::Int(0),
                Value::Bool(true),
                Value::text(conkey),
                Value::text(confkey),
            ]));
        }
        // v7.37 U5 — CHECK constraints (contype 'c'). Previously
        // omitted, so pg_constraint enumeration missed every CHECK.
        let check_names = pg_check_connames(t, tname, &t.schema().checks);
        for conname in check_names {
            rows.push(Row::new(alloc::vec![
                Value::BigInt(next_con_oid()),
                Value::text(conname),
                Value::BigInt(2200),
                Value::text("c"),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(true),
                Value::BigInt(conrelid),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::text(" "),
                Value::text(" "),
                Value::text(" "),
                Value::Bool(true),
                Value::Int(0),
                Value::Bool(true),
                Value::text(String::new()),
                Value::text(String::new()),
            ]));
        }
        // v7.39 (round 210, EXCLUDE Phase 1) — exclusion constraints
        // (contype 'x'). PG's conkey is the constrained columns' attnums
        // (1-based); the operator list rides pg_get_constraintdef, not
        // pg_constraint's columns. conindid points at the backing index
        // (PG creates a real GiST index); SPG has no real index yet
        // (Phase 3), so 0 like the uniqueness rows above.
        for ex in t.schema().exclusion_constraints.iter() {
            let positions: Vec<usize> = ex.elements.iter().map(|(p, _)| *p).collect();
            let conkey_display = conkey_vec(&positions);
            rows.push(Row::new(alloc::vec![
                Value::BigInt(next_con_oid()),
                Value::text(ex.name.clone()),
                Value::BigInt(2200),
                Value::text("x"),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(true),
                Value::BigInt(conrelid),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::text(" "),
                Value::text(" "),
                Value::text(" "),
                Value::Bool(true),
                Value::Int(0),
                Value::Bool(true),
                Value::text(conkey_display),
                Value::text(String::new()),
            ]));
        }
        // v7.37 U6 — NOT NULL constraints (contype 'n', PG 18). PG
        // materialises one row per NOT NULL column, named
        // `<table>_<col>_not_null`, including columns made NOT NULL
        // implicitly by a PRIMARY KEY.
        let pk_cols: alloc::collections::BTreeSet<usize> = t
            .schema()
            .uniqueness_constraints
            .iter()
            .filter(|uc| uc.is_primary_key)
            .flat_map(|uc| uc.columns.iter().copied())
            .collect();
        for (i, col) in cols.iter().enumerate() {
            if col.nullable && !pk_cols.contains(&i) {
                continue;
            }
            let conname = alloc::format!("{tname}_{}_not_null", col.name);
            let conkey_display = alloc::format!("{} [{}]", i + 1, col.name);
            rows.push(Row::new(alloc::vec![
                Value::BigInt(next_con_oid()),
                Value::text(conname),
                Value::BigInt(2200),
                Value::text("n"),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(true),
                Value::BigInt(conrelid),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::text(" "),
                Value::text(" "),
                Value::text(" "),
                Value::Bool(true),
                Value::Int(0),
                Value::Bool(true),
                Value::text(conkey_display),
                Value::text(String::new()),
            ]));
        }
    }
    (schema, rows)
}

/// v7.37.24 (24.8b-3) — map SPG's FK ReferentialAction onto
/// PG's `confupdtype` / `confdeltype` single-char encoding.
fn fk_action_char(action: spg_storage::FkAction) -> &'static str {
    use spg_storage::FkAction as A;
    match action {
        A::NoAction => "a",
        A::Restrict => "r",
        A::Cascade => "c",
        A::SetNull => "n",
        A::SetDefault => "d",
    }
}

/// v7.17.0 Phase 3.P0-55 — synthesise `pg_catalog.pg_database`.
/// SPG is single-database so we surface a single row keyed on the
/// canonical `postgres` database name (matching what every PG
/// admin tool's startup screen expects to find).
pub(crate) fn synth_pg_database(engine: &Engine) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    // v7.39 (round 474) — PG18's pg_database has eighteen columns; this had
    // five, so `SELECT datfrozenxid FROM pg_database` — what a monitoring
    // query asks to watch wraparound — failed outright with "column does
    // not exist".
    //
    // It also named the database `postgres` while `current_database()`
    // answers `spg`, so a client joining the two found no row at all.
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("datname", DataType::Text, false),
        ColumnSchema::new("datdba", DataType::BigInt, false),
        ColumnSchema::new("encoding", DataType::Int, false),
        ColumnSchema::new("datlocprovider", DataType::Text, false),
        ColumnSchema::new("datistemplate", DataType::Bool, false),
        ColumnSchema::new("datallowconn", DataType::Bool, false),
        ColumnSchema::new("dathasloginevt", DataType::Bool, false),
        ColumnSchema::new("datconnlimit", DataType::Int, false),
        ColumnSchema::new("datfrozenxid", DataType::BigInt, false),
        ColumnSchema::new("datminmxid", DataType::BigInt, false),
        ColumnSchema::new("dattablespace", DataType::BigInt, false),
        ColumnSchema::new("datcollate", DataType::Text, false),
        ColumnSchema::new("datctype", DataType::Text, false),
        ColumnSchema::new("datlocale", DataType::Text, true),
        ColumnSchema::new("daticurules", DataType::Text, true),
        ColumnSchema::new("datcollversion", DataType::Text, true),
        ColumnSchema::new("datacl", DataType::Text, true),
    ];
    let rows = alloc::vec![Row::new(alloc::vec![
        Value::BigInt(16384),
        // The name `current_database()` answers, read from the same place
        // it reads, so the two cannot drift apart again.
        Value::text(
            engine
                .session_params
                .get("spg.database")
                .cloned()
                .unwrap_or_else(|| alloc::string::String::from("spg")),
        ),
        Value::BigInt(10),
        Value::Int(6), // UTF8
        // 'c' = libc provider, which is what SPG's C collation is.
        Value::text("c"),
        Value::Bool(false), // datistemplate
        Value::Bool(true),  // datallowconn
        Value::Bool(false), // dathasloginevt
        Value::Int(-1),     // datconnlimit — unlimited
        // The real MVCC floor, not a placeholder: this is the number a
        // wraparound monitor is watching, and SPG has one to give.
        Value::BigInt(i64::try_from(engine.vacuum_oldest_active()).unwrap_or(i64::MAX)),
        Value::BigInt(1), // datminmxid — SPG has no multixact
        Value::BigInt(1663), // dattablespace — pg_default
        Value::text("C"),
        Value::text("C"),
        Value::Null, // datlocale
        Value::Null, // daticurules
        Value::Null, // datcollversion
        Value::Null, // datacl
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
    // v7.39 (read01 round 58) — the three attributes are REAL now. They used to
    // be hard-coded (`false, true, true`) because SPG had no role attributes;
    // a `CREATE ROLE devs NOLOGIN` would still have reported rolcanlogin=true.
    for (i, (name, rec)) in engine.users.iter().enumerate() {
        rows.push(Row::new(alloc::vec![
            Value::BigInt(oid + (i as i64) + 1),
            Value::text(name.to_string()),
            Value::Bool(rec.superuser),
            Value::Bool(rec.inherit),
            Value::Bool(rec.can_login),
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

/// v7.39 (read01 round 58) — synthesise `pg_catalog.pg_auth_members`: one row
/// per role membership (`GRANT devs TO alice`). The oids agree with the ones
/// `synth_pg_roles` hands out, so the canonical
/// `pg_auth_members JOIN pg_roles ON roleid = oid` join resolves.
pub(crate) fn synth_pg_auth_members(engine: &Engine) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("roleid", DataType::BigInt, false),
        ColumnSchema::new("member", DataType::BigInt, false),
        ColumnSchema::new("grantor", DataType::BigInt, false),
        ColumnSchema::new("admin_option", DataType::Bool, false),
    ];
    // Same oid assignment as synth_pg_roles: 11, 12, … in name order.
    let oid_of = |name: &str| -> i64 {
        engine
            .users
            .iter()
            .position(|(n, _)| n == name)
            .map_or(10, |i| 10 + (i as i64) + 1)
    };
    let mut rows: Vec<Row<'static>> = Vec::new();
    for (i, (member, role)) in engine.users.all_memberships().enumerate() {
        rows.push(Row::new(alloc::vec![
            Value::BigInt(200_000 + (i as i64)),
            Value::BigInt(oid_of(role)),
            Value::BigInt(oid_of(member)),
            Value::BigInt(10), // grantor — the bootstrap superuser
            Value::Bool(false),
        ]));
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

/// v7.39 (round 312, V33) — `pg_get_functiondef`'s output: a complete,
/// re-runnable CREATE statement.
///
/// PG's layout, measured byte-for-byte against 18.4 — the continuation
/// lines carry a single leading space, the body is delimited by
/// `$function$`, and there is a trailing newline:
///
/// ```text
/// CREATE OR REPLACE FUNCTION public.f(a integer, b integer)
///  RETURNS integer
///  LANGUAGE sql
/// AS $function$ SELECT a + b $function$
/// ```
///
/// Argument and return types are canonicalised to PG's spelling
/// (`int` → `integer`), because that is what PG prints regardless of how
/// the function was declared.
pub(crate) fn render_function_def(f: &spg_storage::FunctionDef) -> alloc::string::String {
    let args = canonical_arg_list(&f.args_repr);
    let returns = canonical_type_word(f.returns.trim());
    // v7.39 (round 322, V46) — the attribute line, when anything was
    // declared away from PG's defaults. Measured on PG 18.4, the order is
    // volatility, PARALLEL, STRICT, SECURITY DEFINER, LEAKPROOF, COST,
    // ROWS, on its own line between LANGUAGE and AS; an all-default
    // function has no such line at all.
    let attrs = function_attr_words(f);
    let attr_line = if attrs.is_empty() {
        alloc::string::String::new()
    } else {
        alloc::format!(" {}\n", attrs.join(" "))
    };
    alloc::format!(
        "CREATE OR REPLACE FUNCTION public.{}({args})\n RETURNS {returns}\n LANGUAGE {}\n{attr_line}AS $function${}$function$\n",
        f.name,
        f.language,
        f.body,
    )
}

/// v7.39 (round 322, V46) — the declared attribute words in PG's print
/// order. Shared by `pg_get_functiondef`; empty when everything is at its
/// default.
pub(crate) fn function_attr_words(f: &spg_storage::FunctionDef) -> alloc::vec::Vec<alloc::string::String> {
    let mut out = alloc::vec::Vec::new();
    match f.volatility {
        spg_storage::FN_IMMUTABLE => out.push(alloc::string::String::from("IMMUTABLE")),
        spg_storage::FN_STABLE => out.push(alloc::string::String::from("STABLE")),
        _ => {}
    }
    match f.parallel {
        spg_storage::FN_PARALLEL_SAFE => out.push(alloc::string::String::from("PARALLEL SAFE")),
        spg_storage::FN_PARALLEL_RESTRICTED => {
            out.push(alloc::string::String::from("PARALLEL RESTRICTED"));
        }
        _ => {}
    }
    if f.strict {
        out.push(alloc::string::String::from("STRICT"));
    }
    if f.security_definer {
        out.push(alloc::string::String::from("SECURITY DEFINER"));
    }
    if f.leakproof {
        out.push(alloc::string::String::from("LEAKPROOF"));
    }
    if let Some(c) = f.cost {
        out.push(alloc::format!("COST {}", render_fn_number(c)));
    }
    if let Some(r) = f.rows {
        out.push(alloc::format!("ROWS {}", render_fn_number(r)));
    }
    out
}

/// PG prints a whole-numbered COST / ROWS without a decimal point.
fn render_fn_number(v: f64) -> alloc::string::String {
    let whole = v as i64;
    if v.abs() < 1e15 && (whole as f64) == v {
        alloc::format!("{whole}")
    } else {
        alloc::format!("{v}")
    }
}

/// Re-spell a stored argument list (`"(a INT, b INT)"`) the way PG
/// prints it (`"a integer, b integer"`). A name is kept as written; only
/// the type word is canonicalised, and anything unrecognised is left
/// alone rather than guessed at.
fn canonical_arg_list(args_repr: &str) -> alloc::string::String {
    let inner = args_repr.trim().trim_start_matches('(').trim_end_matches(')');
    if inner.trim().is_empty() {
        return alloc::string::String::new();
    }
    inner
        .split(',')
        .map(|part| {
            let part = part.trim();
            match part.split_once(char::is_whitespace) {
                Some((name, ty)) => alloc::format!("{name} {}", canonical_type_word(ty.trim())),
                None => canonical_type_word(part),
            }
        })
        .collect::<alloc::vec::Vec<_>>()
        .join(", ")
}

/// v7.39 (round 339, V63) — the same list with the parameter NAMES
/// dropped: `"(a INT, b TEXT)"` → `"integer,text"`. That is the form
/// `regprocedure` renders and compares by, and PG prints it without a
/// space after the comma (`g(integer,text)`, measured on 18.4).
pub(crate) fn canonical_arg_types(args_repr: &str) -> alloc::string::String {
    let inner = args_repr.trim().trim_start_matches('(').trim_end_matches(')');
    if inner.trim().is_empty() {
        return alloc::string::String::new();
    }
    inner
        .split(',')
        .map(|part| {
            let part = part.trim();
            let ty = part.split_once(char::is_whitespace).map_or(part, |(_, t)| t);
            canonical_type_word(ty.trim())
        })
        .collect::<alloc::vec::Vec<_>>()
        .join(",")
}

/// `int` → `integer`, `TEXT` → `text`. Falls back to the lower-cased
/// original when the word is not a type this engine knows, which keeps
/// user-defined types readable instead of mangling them.
fn canonical_type_word(word: &str) -> alloc::string::String {
    crate::conversions::type_name_to_data_type(word)
        .map_or_else(|| word.to_ascii_lowercase(), pg_type_word)
}

fn pg_type_word(t: DataType) -> alloc::string::String {
    crate::conversions::regtype_oid_to_name(pg_type_oid(t))
        .map_or_else(|| alloc::format!("{t}").to_ascii_lowercase(), Into::into)
}

/// v7.39 (round 312, V33) — one `CREATE RULE` text, shared by
/// `pg_rules.definition` and `pg_get_ruledef`.
///
/// PG's layout is two lines, and the spacing carries meaning: `DO ` is
/// followed by `INSTEAD ` or by a second space, so a DO ALSO rule reads
/// `DO  INSERT …` with the gap where INSTEAD would have gone. Measured
/// against PG 18.4, which is also where the `qualify` split comes from —
/// the default form writes `public.<table>`, the pretty one bare.
/// v7.39 (round 328/V47) — re-deparse ONE rule action from its parse tree,
/// the way PG does, instead of echoing the text the user typed. Measured
/// on PG 18.4:
///
/// ```text
///  INSERT INTO log33 (id, v)
///    VALUES (new.id, new.v)
///  UPDATE r33 SET v = new.v
///    WHERE (r33.id = old.id)
///  DELETE FROM log33
///    WHERE (log33.id = old.id)
/// ```
///
/// So: an INSERT always carries an explicit column list (filled from the
/// catalog when the user omitted it), and VALUES / WHERE start a new line
/// indented by two. A WHERE column with no qualifier is printed qualified
/// by the target table, which is how PG's deparser resolves it against
/// the range table.
///
/// A shape this does not model (`INSERT … SELECT`, whose PG rendering is
/// the multi-line SELECT pretty-printer) keeps the stored text, as does
/// anything that no longer parses.
fn render_rule_action(cmd: &str, cat: Option<&Catalog>) -> alloc::string::String {
    use spg_sql::ast::Statement;
    let Ok(stmt) = spg_sql::parser::parse_statement(cmd) else {
        return alloc::string::String::from(cmd);
    };
    match stmt {
        Statement::Insert(ins) if ins.ctes.is_empty() => {
            let cols = ins.columns.clone().or_else(|| {
                cat.and_then(|c| c.get(&ins.table))
                    .map(|t| t.schema().columns.iter().map(|c| c.name.clone()).collect())
            });
            let Some(cols) = cols else {
                return alloc::string::String::from(cmd);
            };
            let head = alloc::format!("INSERT INTO {} ({})", ins.table, cols.join(", "));
            if let Some(sel) = &ins.select_source {
                // PG renders the SELECT with its multi-line pretty-printer;
                // SPG keeps it on one line (recorded in the ledger). The
                // column list above is PG's either way.
                return alloc::format!("{head}  {sel}");
            }
            let rendered = alloc::format!("{ins}");
            let Some(vpos) = rendered.find(" VALUES ") else {
                return rendered;
            };
            alloc::format!(
                "{head}\n  VALUES {}",
                &rendered[vpos + " VALUES ".len()..]
            )
        }
        Statement::Update(upd) if upd.ctes.is_empty() => {
            let sets: alloc::vec::Vec<alloc::string::String> = upd
                .assignments
                .iter()
                .map(|(c, e)| alloc::format!("{c} = {e}"))
                .collect();
            let mut out = alloc::format!("UPDATE {} SET {}", upd.table, sets.join(", "));
            if let Some(w) = &upd.where_ {
                out.push_str(&alloc::format!(
                    "\n  WHERE {}",
                    qualify_bare_columns(w, &upd.table)
                ));
            }
            out
        }
        Statement::Delete(del) if del.ctes.is_empty() => {
            let mut out = alloc::format!("DELETE FROM {}", del.table);
            if let Some(w) = &del.where_ {
                out.push_str(&alloc::format!(
                    "\n  WHERE {}",
                    qualify_bare_columns(w, &del.table)
                ));
            }
            out
        }
        other => alloc::format!("{other}"),
    }
}

/// Render `e` with every unqualified column reference qualified by
/// `table`, as PG's deparser does once the range table is resolved
/// (`WHERE (r33.id = old.id)`). `new` / `old` keep their own qualifier.
fn qualify_bare_columns(e: &spg_sql::ast::Expr, table: &str) -> alloc::string::String {
    let mut cloned = e.clone();
    qualify_in_place(&mut cloned, table);
    alloc::format!("{cloned}")
}

fn qualify_in_place(e: &mut spg_sql::ast::Expr, table: &str) {
    use spg_sql::ast::Expr;
    let mut stack = alloc::vec![e];
    while let Some(node) = stack.pop() {
        if let Expr::Column(c) = node {
            if c.qualifier.is_none() {
                c.qualifier = Some(alloc::string::String::from(table));
            }
            continue;
        }
        match node {
            Expr::Binary { lhs, rhs, .. } => {
                stack.push(lhs);
                stack.push(rhs);
            }
            Expr::Unary { expr, .. }
            | Expr::IsNull { expr, .. }
            | Expr::BoolTest { expr, .. }
            | Expr::Cast { expr, .. } => stack.push(expr),
            Expr::FunctionCall { args, .. } => stack.extend(args.iter_mut()),
            _ => {}
        }
    }
}

pub(crate) fn render_rule_def(
    r: &spg_storage::RuleDef,
    qualify: bool,
    cat: Option<&Catalog>,
) -> alloc::string::String {
    let table = if qualify {
        alloc::format!("public.{}", r.table)
    } else {
        r.table.clone()
    };
    let mut def = alloc::format!("CREATE RULE {} AS\n    ON {} TO {table}", r.name, r.event);
    if !r.when_condition.is_empty() {
        // PG gives the qualification its own line, indented by three.
        def.push_str("\n   WHERE ");
        def.push_str(&r.when_condition);
    }
    def.push_str(" DO ");
    if r.instead {
        def.push_str("INSTEAD ");
    }
    // Measured on PG 18.4: a deparsed ACTION is preceded by one further
    // space (`DO  INSERT …`, `DO INSTEAD  UPDATE …`) while `NOTHING` is
    // not (`DO INSTEAD NOTHING`).
    if !r.commands.is_empty() {
        def.push(' ');
    } else if !r.instead {
        def.push(' ');
    }
    match r.commands.len() {
        0 => def.push_str("NOTHING"),
        1 => def.push_str(&render_rule_action(&r.commands[0], cat)),
        _ => {
            def.push('(');
            let rendered: alloc::vec::Vec<alloc::string::String> = r
                .commands
                .iter()
                .map(|c| render_rule_action(c, cat))
                .collect();
            def.push_str(&rendered.join("; "));
            def.push(')');
        }
    }
    def.push(';');
    def
}

/// v7.39 (round 312, V33) — `pg_catalog.pg_rewrite`. The rule catalogue
/// itself, which `pg_get_ruledef(oid)` resolves against; without it there
/// was no way to reach a rule by oid at all.
pub(crate) fn synth_pg_rewrite(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("rulename", DataType::Text, false),
        ColumnSchema::new("ev_class", DataType::BigInt, false),
        ColumnSchema::new("ev_type", DataType::Text, false),
        ColumnSchema::new("ev_enabled", DataType::Text, false),
        ColumnSchema::new("is_instead", DataType::Bool, false),
        ColumnSchema::new("ev_qual", DataType::Text, true),
        ColumnSchema::new("ev_action", DataType::Text, true),
    ];
    // `ev_class` has to be the SAME oid pg_class / pg_constraint hand out
    // for that table, or a join against them silently returns nothing.
    let mut names: Vec<String> = cat.visible_table_names();
    names.sort();
    let by_table: alloc::collections::BTreeMap<String, i64> = names
        .iter()
        .enumerate()
        .map(|(i, n)| (n.clone(), 16_384 + i as i64))
        .collect();
    let rows: Vec<Row<'static>> = cat
        .rules()
        .iter()
        .enumerate()
        .map(|(i, r)| {
            // PG's ev_type is a single char: 1 SELECT, 2 UPDATE,
            // 3 INSERT, 4 DELETE.
            let ev_type = match r.event.to_ascii_uppercase().as_str() {
                "SELECT" => "1",
                "UPDATE" => "2",
                "INSERT" => "3",
                _ => "4",
            };
            Row::new(alloc::vec![
                Value::BigInt(RULE_OID_BASE + i as i64),
                Value::text(r.name.clone()),
                Value::BigInt(*by_table.get(&r.table).unwrap_or(&0)),
                Value::text(ev_type),
                Value::text("O"),
                Value::Bool(r.instead),
                if r.when_condition.is_empty() {
                    Value::Null
                } else {
                    Value::text(r.when_condition.clone())
                },
                Value::text(r.commands.join("; ")),
            ])
        })
        .collect();
    (schema, rows)
}

/// The oid band `pg_rewrite` rows occupy, kept away from the other synth
/// catalogues so a rule oid can only ever resolve to a rule.
pub(crate) const RULE_OID_BASE: i64 = 500_000;

/// v7.39 (round 143) — synthesise `pg_catalog.pg_rules`: one row per
/// catalogued query-rewrite RULE. PG's `definition` column is
/// `pg_get_ruledef`'s pretty-printed deparse; SPG reconstructs the canonical
/// single-line CREATE RULE text from the stored `RuleDef` (the same fidelity
/// level as `pg_views.definition`, which surfaces the stored view body).
pub(crate) fn synth_pg_rules(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("schemaname", DataType::Text, false),
        ColumnSchema::new("tablename", DataType::Text, false),
        ColumnSchema::new("rulename", DataType::Text, false),
        ColumnSchema::new("definition", DataType::Text, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for r in cat.rules() {
        // v7.39 (round 312) — `pg_rules.definition` IS `pg_get_ruledef`'s
        // default form, schema qualification and all (measured: PG shows
        // `public.r33` here, and drops it only for the pretty spelling).
        let def = render_rule_def(r, true, Some(cat));
        rows.push(Row::new(alloc::vec![
            Value::text("public"),
            Value::text(r.table.clone()),
            Value::text(r.name.clone()),
            Value::text(def),
        ]));
    }
    (schema, rows)
}

pub(crate) fn synth_pg_views(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("schemaname", DataType::Text, false),
        ColumnSchema::new("viewname", DataType::Text, false),
        ColumnSchema::new("definition", DataType::Text, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for (name, def) in cat.views_all() {
        let Some(name) = cat.listed_name(name) else {
            continue;
        };
        rows.push(Row::new(alloc::vec![
            Value::text("public"),
            Value::text(name.to_string()),
            Value::text(def.body.clone()),
        ]));
    }
    (schema, rows)
}

/// v7.38 (read01 P3.22/P3.23) — the canonical GUC inventory:
/// `(name, boot_val, category, vartype, context)`. Single source of truth
/// shared by `pg_settings`, `SHOW <name>`, and `SHOW ALL` so all three
/// agree on which parameters exist and their defaults. vartype is
/// annotated (not inferred) because SPG stores memory / duration settings
/// in human form ("4MB") where inference would read "string" — PG
/// classifies work_mem as integer.
pub(crate) fn canonical_gucs() -> &'static [(
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
)] {
    &[
        // v7.39 (round 474) — `synchronous_commit` is a REAL durability
        // control here (round 171 gated the WAL-fsync wait on it, and the
        // fair panel drives both engines through it), but neither
        // `SHOW synchronous_commit` nor `pg_settings` admitted it existed:
        // a client that set it and read it back was told the parameter is
        // not recognised.
        //
        // Metadata copied from PG18: `on | Write-Ahead Log / Settings |
        // enum | user`.
        //
        // Nothing else was added. SPG lists thirty parameters because it
        // implements thirty; PG18's other 368 are knobs SPG does not read,
        // and reporting one would tell a tuning tool that turning it does
        // something. `enable_seqscan` is the case in point — SET validates
        // it and nothing ever reads it, so it stays out.
        (
            "synchronous_commit",
            "on",
            "Write-Ahead Log / Settings",
            "enum",
            "user",
        ),
        (
            "server_version",
            "18.4 (spg)",
            "Preset Options",
            "string",
            "internal",
        ),
        (
            "server_version_num",
            "180004",
            "Preset Options",
            "integer",
            "internal",
        ),
        (
            "server_encoding",
            "UTF8",
            "Client Connection Defaults",
            "string",
            "internal",
        ),
        (
            "client_encoding",
            "UTF8",
            "Client Connection Defaults",
            "string",
            "user",
        ),
        (
            "DateStyle",
            "ISO, MDY",
            "Client Connection Defaults",
            "string",
            "user",
        ),
        (
            "TimeZone",
            "UTC",
            "Client Connection Defaults",
            "string",
            "user",
        ),
        (
            "IntervalStyle",
            "postgres",
            "Client Connection Defaults",
            "enum",
            "user",
        ),
        (
            // v7.39 (read01 round 44) — PG's initdb default for a UTF-8 /
            // english locale. SPG's FTS pipeline resolves an unset value
            // to the english config so bare to_tsvector / to_tsquery stem
            // and drop stopwords like PG out of the box.
            "default_text_search_config",
            "pg_catalog.english",
            "Client Connection Defaults",
            "string",
            "user",
        ),
        (
            "extra_float_digits",
            "1",
            "Client Connection Defaults",
            "integer",
            "user",
        ),
        (
            "bytea_output",
            "hex",
            "Client Connection Defaults",
            "enum",
            "user",
        ),
        (
            "standard_conforming_strings",
            "on",
            "Compatibility",
            "bool",
            "user",
        ),
        (
            "integer_datetimes",
            "on",
            "Compatibility",
            "bool",
            "internal",
        ),
        (
            "max_connections",
            "100",
            "Connections and Authentication",
            "integer",
            "postmaster",
        ),
        (
            "lock_timeout",
            "0",
            "Client Connection Defaults",
            "integer",
            "user",
        ),
        (
            "idle_in_transaction_session_timeout",
            "0",
            "Client Connection Defaults",
            "integer",
            "user",
        ),
        (
            "transaction_timeout",
            "0",
            "Client Connection Defaults",
            "integer",
            "user",
        ),
        (
            "statement_timeout",
            "0",
            "Client Connection Defaults",
            "integer",
            "user",
        ),
        (
            "client_min_messages",
            "notice",
            "Client Connection Defaults",
            "enum",
            "user",
        ),
        (
            "default_tablespace",
            "",
            "Client Connection Defaults",
            "string",
            "user",
        ),
        (
            "default_table_access_method",
            "heap",
            "Client Connection Defaults",
            "string",
            "user",
        ),
        (
            "row_security",
            "on",
            "Client Connection Defaults",
            "bool",
            "user",
        ),
        (
            "check_function_bodies",
            "on",
            "Client Connection Defaults",
            "bool",
            "user",
        ),
        (
            "xmloption",
            "content",
            "Client Connection Defaults",
            "enum",
            "user",
        ),
        (
            "work_mem",
            "4MB",
            "Resource Usage / Memory",
            "integer",
            "user",
        ),
        (
            "maintenance_work_mem",
            "64MB",
            "Resource Usage / Memory",
            "integer",
            "user",
        ),
        (
            "shared_buffers",
            "128MB",
            "Resource Usage / Memory",
            "integer",
            "postmaster",
        ),
        (
            "effective_cache_size",
            "4GB",
            "Query Tuning / Planner Cost Constants",
            "integer",
            "user",
        ),
        (
            "search_path",
            "\"$user\", public",
            "Client Connection Defaults",
            "string",
            "user",
        ),
        (
            "application_name",
            "",
            "Reporting and Logging",
            "string",
            "user",
        ),
        (
            "default_transaction_isolation",
            "read committed",
            "Client Connection Defaults",
            "enum",
            "user",
        ),
    ]
}

/// v7.39 (round 502) — synthesise `pg_catalog.pg_timezone_names`.
///
/// PG's shape is `(name, abbrev, utc_offset interval, is_dst bool)`, and
/// it is how a client populates a timezone picker. SPG resolved named
/// zones correctly — round 502 measured `America/New_York` rendering
/// -05 in January and -04 in July, byte-identical to PG18 — but answered
/// "relation pg_timezone_names does not exist" when asked to list them.
///
/// The offsets are taken at NOW, exactly as PG does: a zone's offset and
/// DST flag depend on the instant, and PG reports the current one.
pub(crate) fn synth_pg_timezone_names(engine: &Engine) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("name", DataType::Text, false),
        ColumnSchema::new("abbrev", DataType::Text, false),
        ColumnSchema::new("utc_offset", DataType::Interval, false),
        ColumnSchema::new("is_dst", DataType::Bool, false),
    ];
    // The clock is the engine's, so a test that froze it sees a stable
    // view; without one, epoch — the offsets barely move either way.
    let now = engine.clock.map_or(0, |f| f());
    let rows = engine
        .tz_all_at(now)
        .into_iter()
        .map(|(name, abbrev, off_secs, is_dst)| {
            Row::new(alloc::vec![
                Value::text(name),
                Value::text(abbrev),
                Value::Interval {
                    months: 0,
                    days: 0,
                    micros: off_secs * 1_000_000,
                },
                Value::Bool(is_dst),
            ])
        })
        .collect();
    (schema, rows)
}

/// v7.39 (round 502) — `pg_catalog.pg_timezone_abbrevs`, the same data
/// keyed by designation. PG lists each abbreviation once; zones sharing
/// one (every US Eastern zone reports `EST`) collapse, so this dedups on
/// the abbreviation and keeps the first offset seen in name order.
pub(crate) fn synth_pg_timezone_abbrevs(engine: &Engine) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("abbrev", DataType::Text, false),
        ColumnSchema::new("utc_offset", DataType::Interval, false),
        ColumnSchema::new("is_dst", DataType::Bool, false),
    ];
    // The clock is the engine's, so a test that froze it sees a stable
    // view; without one, epoch — the offsets barely move either way.
    let now = engine.clock.map_or(0, |f| f());
    let mut seen: alloc::collections::BTreeMap<alloc::string::String, (i64, bool)> =
        alloc::collections::BTreeMap::new();
    for (_, abbrev, off_secs, is_dst) in engine.tz_all_at(now) {
        seen.entry(abbrev).or_insert((off_secs, is_dst));
    }
    let rows = seen
        .into_iter()
        .map(|(abbrev, (off_secs, is_dst))| {
            Row::new(alloc::vec![
                Value::text(abbrev),
                Value::Interval {
                    months: 0,
                    days: 0,
                    micros: off_secs * 1_000_000,
                },
                Value::Bool(is_dst),
            ])
        })
        .collect();
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-57 — synthesise `pg_catalog.pg_settings`. ORM
/// connection-checkers (sqlx pre-flight, Diesel migrator) and admin
/// tools read `pg_settings` to discover server-side configuration.
/// SPG surfaces every session_param + a small set of canonical PG
/// defaults so the pre-flight queries match.
pub(crate) fn synth_pg_settings(engine: &Engine) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    // v7.38 (read01 P3.22) — PG 18's full 17-column pg_settings shape so
    // admin tools that filter on context / vartype / source (pgAdmin's
    // parameter editor, postgres_exporter) get the columns they expect.
    let schema = alloc::vec![
        ColumnSchema::new("name", DataType::Text, false),
        ColumnSchema::new("setting", DataType::Text, false),
        ColumnSchema::new("unit", DataType::Text, true),
        ColumnSchema::new("category", DataType::Text, false),
        ColumnSchema::new("short_desc", DataType::Text, true),
        ColumnSchema::new("extra_desc", DataType::Text, true),
        ColumnSchema::new("context", DataType::Text, false),
        ColumnSchema::new("vartype", DataType::Text, false),
        ColumnSchema::new("source", DataType::Text, false),
        ColumnSchema::new("min_val", DataType::Text, true),
        ColumnSchema::new("max_val", DataType::Text, true),
        ColumnSchema::new("enumvals", DataType::Text, true),
        ColumnSchema::new("boot_val", DataType::Text, true),
        ColumnSchema::new("reset_val", DataType::Text, true),
        ColumnSchema::new("sourcefile", DataType::Text, true),
        ColumnSchema::new("sourceline", DataType::Int, true),
        ColumnSchema::new("pending_restart", DataType::Bool, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    let defaults = canonical_gucs();
    // Build a full 17-column row. `setting` honours session overrides;
    // `source` reflects whether the value came from a SET; `boot_val` /
    // `reset_val` stay at the compiled-in default.
    let mut push = |name: &str, boot: &str, cat: &str, vartype: &str, context: &str| {
        let overridden = engine
            .session_params
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone());
        let source = if overridden.is_some() {
            "session"
        } else {
            "default"
        };
        let setting = overridden.unwrap_or_else(|| boot.into());
        // v7.39 (GUC knife 2) — PG reports ms-unit time GUCs as the bare
        // number in `setting` with `unit = 'ms'` ("7s" -> 7000 | ms).
        // v7.39 (round 522) — and MEMORY GUCs the same way: `work_mem`
        // reads `4096 | kB`, not `4MB`. Reporting the human form while
        // `vartype` says `integer` contradicts itself — a client that
        // believes the vartype and parses the setting gets nothing.
        // `boot_val` / `reset_val` are raw too (measured: they stay
        // `4096` across a `SET work_mem = '8MB'`).
        let raw = |v: &str| crate::session::guc_raw_setting(name, v);
        let unit = crate::session::guc_unit(name)
            .map_or(Value::Null, |u| Value::text::<String>(u.into()));
        let setting = raw(&setting).unwrap_or(setting);
        let boot_raw = raw(boot).unwrap_or_else(|| boot.into());
        rows.push(Row::new(alloc::vec![
            Value::text::<String>(name.into()),
            Value::text(setting),
            unit, // ms for time GUCs; else self-describing
            Value::text::<String>(cat.into()),
            Value::Null, // short_desc
            Value::Null, // extra_desc
            Value::text::<String>(context.into()),
            Value::text::<String>(vartype.into()),
            Value::text::<String>(source.into()),
            Value::Null, // min_val
            Value::Null, // max_val
            Value::Null, // enumvals
            Value::text(boot_raw.clone()),
            Value::text(boot_raw), // reset_val = boot_val
            Value::Null,                        // sourcefile
            Value::Null,                        // sourceline
            Value::Bool(false),                 // pending_restart
        ]));
    };
    for &(name, val, cat, vartype, context) in defaults {
        push(name, val, cat, vartype, context);
    }
    // Session-set params not in the canonical list get their own rows;
    // vartype is inferred from the value and source is always "session".
    for (k, v) in &engine.session_params {
        if defaults.iter().any(|(n, ..)| (*n).eq_ignore_ascii_case(k)) {
            continue;
        }
        // v7.39 (GUC knife 2) — customised (dotted) parameters are NOT
        // rows of PG's pg_settings (only registered extension GUCs are);
        // they remain readable via current_setting().
        if k.contains('.') {
            continue;
        }
        let vartype = infer_guc_vartype(v);
        rows.push(Row::new(alloc::vec![
            Value::text(k.clone()),
            Value::text(v.clone()),
            Value::Null,
            Value::text::<String>("Session".into()),
            Value::Null,
            Value::Null,
            Value::text::<String>("user".into()),
            Value::text::<String>(vartype.into()),
            Value::text::<String>("session".into()),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::text(v.clone()),
            Value::text(v.clone()),
            Value::Null,
            Value::Null,
            Value::Bool(false),
        ]));
    }
    (schema, rows)
}

/// v7.38 (read01 P3.22) — best-effort `vartype` for a session GUC whose
/// type SPG doesn't track: bool for on/off/true/false, integer/real for
/// numeric text, string otherwise. Annotated defaults bypass this.
fn infer_guc_vartype(v: &str) -> &'static str {
    match v.trim().to_ascii_lowercase().as_str() {
        "on" | "off" | "true" | "false" => "bool",
        s if s.parse::<i64>().is_ok() => "integer",
        s if s.parse::<f64>().is_ok() => "real",
        _ => "string",
    }
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
/// v7.39 — `pg_catalog.pg_tables` (the convenience view PG ships).
/// One row per user table; replaces the pgwire canned response so
/// projections / WHERE / JOINs work like any relation.
pub(crate) fn synth_pg_tables(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("schemaname", DataType::Text, false),
        ColumnSchema::new("tablename", DataType::Text, false),
        ColumnSchema::new("tableowner", DataType::Text, false),
        ColumnSchema::new("tablespace", DataType::Text, true),
        ColumnSchema::new("hasindexes", DataType::Bool, false),
        ColumnSchema::new("hasrules", DataType::Bool, false),
        ColumnSchema::new("hastriggers", DataType::Bool, false),
        ColumnSchema::new("rowsecurity", DataType::Bool, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for tname in cat.visible_table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        let has_indexes = !t.indices().is_empty() || !t.schema().uniqueness_constraints.is_empty();
        rows.push(Row::new(alloc::vec![
            Value::text("public"),
            Value::text(tname.clone()),
            Value::text("admin"),
            Value::Null,
            Value::Bool(has_indexes),
            Value::Bool(false),
            Value::Bool(cat.triggers().iter().any(|tg| tg.table == tname)),
            Value::Bool(t.schema().row_security),
        ]));
    }
    (schema, rows)
}

/// v7.39 (read01 round 50) — synthesise `pg_catalog.pg_description` from the
/// catalog's COMMENT store. `objsubid` is the 1-based column number for a
/// column comment, 0 otherwise; `classoid` is pg_class (1259) for relations
/// and their columns, 0 for the kinds SPG stores by name only.
/// v7.39 (read01 round 57) — `information_schema.role_table_grants` (and, with
/// the same shape, `.table_privileges`). One row per (grantee, privilege): the
/// owner's implicit full set, plus every explicit GRANT recorded in the table's
/// ACL. Round 51 hard-coded the owner's seven and stopped there — there were no
/// grants to report, because GRANT was a no-op.
pub(crate) fn synth_info_role_table_grants(
    cat: &Catalog,
    grantee: &str,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let cols = alloc::vec![
        ColumnSchema::new("grantor", DataType::Text, false),
        ColumnSchema::new("grantee", DataType::Text, false),
        ColumnSchema::new("table_catalog", DataType::Text, false),
        ColumnSchema::new("table_schema", DataType::Text, false),
        ColumnSchema::new("table_name", DataType::Text, false),
        ColumnSchema::new("privilege_type", DataType::Text, false),
        ColumnSchema::new("is_grantable", DataType::Text, false),
        ColumnSchema::new("with_hierarchy", DataType::Text, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    let _ = grantee;
    for tname in cat.visible_table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        let sc = t.schema();
        let owner = sc
            .owner
            .clone()
            .unwrap_or_else(|| alloc::string::String::from(crate::session::LOGIN_ROLE));
        // An un-granted table reports only the owner's implicit set; once
        // relacl materialises, the ACL itself is the whole truth (its first
        // entry IS the owner's).
        let acl: Vec<(alloc::string::String, u16, u16, alloc::string::String)> =
            if sc.acl.is_empty() {
                alloc::vec![(
                    owner.clone(),
                    spg_storage::priv_bits::ALL,
                    spg_storage::priv_bits::ALL,
                    owner.clone(),
                )]
            } else {
                sc.acl
                    .iter()
                    .map(|a| {
                        // The owner's own row is grantable throughout; a grantee's
                        // is grantable only where WITH GRANT OPTION was given.
                        let grantable = if a.grantee.eq_ignore_ascii_case(&owner) {
                            a.privs
                        } else {
                            a.grantable
                        };
                        (a.grantee.clone(), a.privs, grantable, a.grantor.clone())
                    })
                    .collect()
            };
        for (who, privs, grantable, grantor) in acl {
            // information_schema is the SQL standard's view, so it lists only
            // the standard privileges — PG's non-standard MAINTAIN shows up in
            // relacl (`m`) but never here.
            for bit in crate::acl::priv_iter(privs & !spg_storage::priv_bits::MAINTAIN) {
                let word = crate::acl::priv_word(bit);
                rows.push(Row::new(alloc::vec![
                    Value::text(grantor.clone()),
                    Value::text(if who.is_empty() {
                        alloc::string::String::from("PUBLIC")
                    } else {
                        who.clone()
                    }),
                    Value::text(alloc::string::String::from("app")),
                    Value::text(alloc::string::String::from("public")),
                    Value::text(tname.clone()),
                    Value::text(alloc::string::String::from(word)),
                    Value::text(alloc::string::String::from(if grantable & bit != 0 {
                        "YES"
                    } else {
                        "NO"
                    },)),
                    // PG sets with_hierarchy YES only for SELECT.
                    Value::text(alloc::string::String::from(
                        if bit == spg_storage::priv_bits::SELECT {
                            "YES"
                        } else {
                            "NO"
                        },
                    )),
                ]));
            }
        }
    }
    (cols, rows)
}

/// v7.39 (read01 round 59) — `information_schema.column_privileges`: one row per
/// (column, grantee, privilege). PG lists a column's own grants here; the
/// table-wide ones live in `table_privileges`.
pub(crate) fn synth_info_column_privileges(
    cat: &Catalog,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let cols = alloc::vec![
        ColumnSchema::new("grantor", DataType::Text, false),
        ColumnSchema::new("grantee", DataType::Text, false),
        ColumnSchema::new("table_catalog", DataType::Text, false),
        ColumnSchema::new("table_schema", DataType::Text, false),
        ColumnSchema::new("table_name", DataType::Text, false),
        ColumnSchema::new("column_name", DataType::Text, false),
        ColumnSchema::new("privilege_type", DataType::Text, false),
        ColumnSchema::new("is_grantable", DataType::Text, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for tname in cat.visible_table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        for col in &t.schema().columns {
            for a in &col.acl {
                for bit in crate::acl::priv_iter(a.privs & !spg_storage::priv_bits::MAINTAIN) {
                    rows.push(Row::new(alloc::vec![
                        Value::text(a.grantor.clone()),
                        Value::text(if a.grantee.is_empty() {
                            alloc::string::String::from("PUBLIC")
                        } else {
                            a.grantee.clone()
                        }),
                        Value::text(alloc::string::String::from("app")),
                        Value::text(alloc::string::String::from("public")),
                        Value::text(tname.clone()),
                        Value::text(col.name.clone()),
                        Value::text(alloc::string::String::from(crate::acl::priv_word(bit))),
                        Value::text(alloc::string::String::from(if a.grantable & bit != 0 {
                            "YES"
                        } else {
                            "NO"
                        },)),
                    ]));
                }
            }
        }
    }
    (cols, rows)
}

pub(crate) fn synth_pg_description(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let cols = alloc::vec![
        ColumnSchema::new("objoid", DataType::Int, false),
        ColumnSchema::new("classoid", DataType::Int, false),
        ColumnSchema::new("objsubid", DataType::Int, false),
        ColumnSchema::new("description", DataType::Text, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for (key, text) in cat.comments() {
        let Some((kind, name)) = key.split_once(':') else {
            continue;
        };
        let (relname, subid) = if kind == "column" {
            match name.split_once('.') {
                Some((t, c)) => {
                    let sub = cat
                        .get(t)
                        .and_then(|tb| {
                            tb.schema()
                                .columns
                                .iter()
                                .position(|sc| sc.name.eq_ignore_ascii_case(c))
                        })
                        .map_or(0, |p| i32::try_from(p + 1).unwrap_or(0));
                    (t, sub)
                }
                None => continue,
            }
        } else {
            (name, 0)
        };
        let is_relation = matches!(kind, "table" | "view" | "index" | "sequence" | "column");
        let objoid = if is_relation {
            crate::eval::regclass_name_to_oid(cat, relname)
                .and_then(|o| i32::try_from(o).ok())
                .unwrap_or(0)
        } else {
            0
        };
        let classoid = if is_relation { 1259 } else { 0 };
        rows.push(Row::new(alloc::vec![
            Value::Int(objoid),
            Value::Int(classoid),
            Value::Int(subid),
            Value::text(text.clone()),
        ]));
    }
    (cols, rows)
}

/// v7.39 (read01 round 83) — the ONE `CREATE [UNIQUE] INDEX …` renderer, PG's
/// `pg_get_indexdef` spelling. Both `pg_indexes.indexdef` (the catalog view) and
/// the `pg_get_indexdef(regclass)` function had their own copy; the function's
/// was the poorer one — it ignored `idx.expression` (so `lower(name)` came back
/// as `name`), ignored the constraint-backing check (so a primary key's index
/// printed `CREATE INDEX` instead of `CREATE UNIQUE INDEX`), and only ever
/// listed column names. One renderer now feeds both.
pub(crate) fn render_indexdef(
    t: &spg_storage::Table,
    idx: &spg_storage::Index,
    tname: &str,
) -> alloc::string::String {
    let col_at = |pos: usize| -> alloc::string::String {
        t.schema()
            .columns
            .get(pos)
            .map_or_else(|| "?".into(), |c| c.name.clone())
    };
    let mut positions = alloc::vec![idx.column_position];
    positions.extend(idx.extra_column_positions.iter().copied());
    let cols = positions
        .iter()
        .map(|&p| col_at(p))
        .collect::<Vec<_>>()
        .join(", ");
    // v7.39 (read01 round 83) — an index prints UNIQUE only when it is the one
    // that ENFORCES a uniqueness constraint, not merely when its columns happen
    // to match one. PG: `CREATE INDEX idx ON t(a)` over a table that also has
    // `UNIQUE(a)` is a PLAIN index — the constraint is enforced by its own
    // auto-created index (`t_a_key` / `t_pkey`), a different relation. SPG
    // enforces uniqueness through the constraint rather than the index flag, so
    // the witness is the auto-index NAMING PG uses: `<table>_pkey` for a primary
    // key, `<table>_<col…>_key` for a UNIQUE. Matching by column set alone (the
    // old test) mislabelled every user index that shadowed a constrained column.
    let col_name_at = |pos: usize| -> alloc::string::String {
        t.schema()
            .columns
            .get(pos)
            .map_or_else(|| "?".into(), |c| c.name.clone())
    };
    let backs_unique_constraint = t.schema().uniqueness_constraints.iter().any(|uc| {
        if uc.columns.len() != positions.len() || !positions.iter().all(|p| uc.columns.contains(p))
        {
            return false;
        }
        let auto_name = if uc.is_primary_key {
            alloc::format!("{tname}_pkey")
        } else {
            let cols_part = uc
                .columns
                .iter()
                .map(|&p| col_name_at(p))
                .collect::<Vec<_>>()
                .join("_");
            alloc::format!("{tname}_{cols_part}_key")
        };
        idx.name == auto_name
    });
    let unique_kw = if idx.is_unique || backs_unique_constraint {
        "UNIQUE "
    } else {
        ""
    };
    // The key list is the EXPRESSION when the index has one.
    // v7.39 (read01 round 83) — PG double-parenthesises an operator expression
    // key (`((a + b))`) but not a bare function call (`abs(a)`, `lower(name)`).
    // SPG stores a binary/unary expression's Display form already wrapped in one
    // pair (`(a + b)`), so add the outer pair exactly when the stored form opens
    // with `(` — which a function call never does.
    let key = match &idx.expression {
        Some(expr) if expr.starts_with('(') => alloc::format!("({expr})"),
        Some(expr) => expr.clone(),
        None => cols,
    };
    // v7.39 (round 473) — `NULLS NOT DISTINCT` sits after the key list and
    // before WHERE, measured on PG18:
    //   CREATE UNIQUE INDEX pix ON public.p USING btree (a)
    //   NULLS NOT DISTINCT WHERE (b > 0)
    // The index has enforced this since round 52; the definition it reports
    // did not carry it, so a dump / restore silently dropped the semantics
    // and rows the original refused became acceptable in the copy.
    let nnd = if idx.nulls_not_distinct {
        " NULLS NOT DISTINCT"
    } else {
        ""
    };
    // v7.39 (round 475) — the access method the index actually is. This was
    // the literal `btree` for every index, so a GIN index reported itself as
    // a btree — and a dump of it restored as one.
    let am = match &idx.kind {
        spg_storage::IndexKind::Gin(_)
        | spg_storage::IndexKind::GinTrgm(_)
        | spg_storage::IndexKind::GinFulltext(_)
        | spg_storage::IndexKind::GinJsonb(_) => "gin",
        spg_storage::IndexKind::Brin { .. } => "brin",
        spg_storage::IndexKind::Nsw(_) => "hnsw",
        spg_storage::IndexKind::BTree(_) => "btree",
    };
    match &idx.partial_predicate {
        Some(pred) => {
            let p = pred.trim();
            let wrapped = if p.starts_with('(') && p.ends_with(')') {
                alloc::string::String::from(p)
            } else {
                alloc::format!("({p})")
            };
            alloc::format!(
                "CREATE {unique_kw}INDEX {} ON public.{tname} USING {am} ({key}){nnd} WHERE {wrapped}",
                idx.name,
            )
        }
        None => alloc::format!(
            "CREATE {unique_kw}INDEX {} ON public.{tname} USING {am} ({key}){nnd}",
            idx.name,
        ),
    }
}

pub(crate) fn synth_pg_indexes(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("schemaname", DataType::Text, false),
        ColumnSchema::new("tablename", DataType::Text, false),
        ColumnSchema::new("indexname", DataType::Text, false),
        ColumnSchema::new("indexdef", DataType::Text, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for tname in cat.visible_table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        for idx in t.indices() {
            let indexdef = render_indexdef(t, idx, &tname);
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
    // v7.37.24 (24.8b-2) — widened from 5 to 19 PG-canonical
    // columns. The PG-`pg_index` shape is what pgAdmin's index
    // explorer + sqlx introspection query, so the columns
    // dashboards walk are populated from live catalog state.
    let schema = alloc::vec![
        ColumnSchema::new("indexrelid", DataType::BigInt, false),
        ColumnSchema::new("indrelid", DataType::BigInt, false),
        ColumnSchema::new("indnatts", DataType::SmallInt, false),
        ColumnSchema::new("indnkeyatts", DataType::SmallInt, false),
        ColumnSchema::new("indisunique", DataType::Bool, false),
        ColumnSchema::new("indnullsnotdistinct", DataType::Bool, false),
        ColumnSchema::new("indisprimary", DataType::Bool, false),
        ColumnSchema::new("indisexclusion", DataType::Bool, false),
        ColumnSchema::new("indimmediate", DataType::Bool, false),
        ColumnSchema::new("indisclustered", DataType::Bool, false),
        ColumnSchema::new("indisvalid", DataType::Bool, false),
        ColumnSchema::new("indcheckxmin", DataType::Bool, false),
        ColumnSchema::new("indisready", DataType::Bool, false),
        ColumnSchema::new("indislive", DataType::Bool, false),
        ColumnSchema::new("indisreplident", DataType::Bool, false),
        ColumnSchema::new("indkey", DataType::Text, false),
        ColumnSchema::new("indcollation", DataType::Text, false),
        ColumnSchema::new("indclass", DataType::Text, false),
        ColumnSchema::new("indoption", DataType::Text, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    let mut idx_oid: i64 = 100_000;
    // Build a name → user-relation OID map so indrelid matches
    // pg_class.oid (synth_pg_class starts at 16384). Without
    // this, joins between pg_index and pg_class fail.
    let names = cat.visible_table_names();
    let mut table_oid: i64 = 16384;
    let mut by_table: alloc::collections::BTreeMap<String, i64> =
        alloc::collections::BTreeMap::new();
    for tname in &names {
        by_table.insert(tname.clone(), table_oid);
        table_oid = table_oid.saturating_add(1);
    }
    for tname in &names {
        let Some(t) = cat.get(tname) else { continue };
        let relid = *by_table.get(tname).unwrap_or(&0);
        for idx in t.indices() {
            idx_oid += 1;
            let n_attrs_total = 1 + idx.extra_column_positions.len();
            // Build PG's `indkey` int2vector — space-separated
            // column positions, 1-based. SPG stores positions
            // 0-based; add 1 to align with PG's attnum.
            let mut indkey = alloc::string::String::new();
            indkey.push_str(&alloc::format!("{}", idx.column_position + 1));
            for extra in &idx.extra_column_positions {
                indkey.push(' ');
                indkey.push_str(&alloc::format!("{}", extra + 1));
            }
            // indclass: array of opclass OIDs, one per column.
            // SPG uses default opclass for every column; PG would
            // emit `1978 1978` for two int4 columns. We populate
            // with placeholder 0s so the shape stays valid.
            let mut indclass = alloc::string::String::new();
            let mut indcollation = alloc::string::String::new();
            let mut indoption = alloc::string::String::new();
            for i in 0..n_attrs_total {
                if i > 0 {
                    indclass.push(' ');
                    indcollation.push(' ');
                    indoption.push(' ');
                }
                indclass.push('0');
                indcollation.push('0');
                indoption.push('0');
            }
            let is_primary = idx.name.ends_with("_pkey");
            let is_partial = idx.partial_predicate.is_some();
            let is_expression = idx.expression.is_some();
            let _ = (is_partial, is_expression);
            rows.push(Row::new(alloc::vec![
                Value::BigInt(idx_oid),
                Value::BigInt(relid),
                Value::SmallInt(i16::try_from(n_attrs_total).unwrap_or(i16::MAX)),
                Value::SmallInt(i16::try_from(n_attrs_total).unwrap_or(i16::MAX)),
                Value::Bool(idx.is_unique),
                // v7.39 (round 473) — the index has carried this since
                // round 52 and enforces it; only the catalog was still
                // answering `f`, so a migration tool reading pg_index saw
                // a plain unique index and would have tried to "fix" it.
                Value::Bool(idx.nulls_not_distinct),
                Value::Bool(is_primary),
                Value::Bool(false), // indisexclusion — EXCLUDE constraint
                Value::Bool(true),  // indimmediate
                Value::Bool(false), // indisclustered
                Value::Bool(true),  // indisvalid
                Value::Bool(false), // indcheckxmin
                Value::Bool(true),  // indisready
                Value::Bool(true),  // indislive
                Value::Bool(false), // indisreplident
                Value::text(indkey),
                Value::text(indcollation),
                Value::text(indclass),
                Value::text(indoption),
            ]));
        }
    }
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-52 — synthesise `pg_catalog.pg_namespace`.
/// SPG is single-schema so we expose the canonical PG schemas:
/// `public` (user-facing), `pg_catalog` (built-in), and
/// `information_schema` (PG meta).
pub(crate) fn synth_pg_namespace(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("nspname", DataType::Text, false),
        ColumnSchema::new("nspowner", DataType::BigInt, false),
        // v7.39 (read01 round 60) — the schema ACL. Never NULL for `public`:
        // PG ships it with PUBLIC holding USAGE (but NOT create).
        ColumnSchema::new("nspacl", DataType::Text, true),
    ];
    let public_acl = crate::acl::render_nspacl(cat);
    let rows = alloc::vec![
        Row::new(alloc::vec![
            Value::BigInt(11),
            Value::text("pg_catalog"),
            Value::BigInt(10),
            Value::Null,
        ]),
        Row::new(alloc::vec![
            Value::BigInt(2200),
            Value::text("public"),
            Value::BigInt(10),
            Value::text(public_acl),
        ]),
        Row::new(alloc::vec![
            Value::BigInt(13000),
            Value::text("information_schema"),
            Value::BigInt(10),
            Value::Null,
        ]),
    ];
    (schema, rows)
}

/// v7.16.2 — drop the synthesised meta view into the enriched
/// catalog so the regular FROM-resolution path can see it.
/// v7.39 (round 313, V34) — pg_catalog's identifier columns are typed
/// `name`, not `text`, so `pg_typeof(relname)` answers `name`. Every
/// synth view built them as Text, and reflection tooling that keys off
/// the reported type saw the wrong one.
///
/// The list is EXHAUSTIVE and taken from PG 18.4 itself — the columns
/// whose `pg_type.typname` is `name`, intersected with the views this
/// engine synthesises. It is not derived from the column NAME, because
/// that does not work: `pg_config.name`, `pg_cursors.name` and
/// `pg_backend_memory_contexts.name` are all `text`, and every
/// `*namespace` is an oid. Guessing would have retyped those too.
///
/// Applied here, at the single point every synth view passes through,
/// rather than at each of the sixty column definitions — one place to
/// audit against the catalogue, and a new view cannot forget to do it.
///
/// information_schema is deliberately absent: PG types its identifier
/// columns `information_schema.sql_identifier`, a DOMAIN over name, and
/// its other columns other domains again (`yes_or_no`,
/// `cardinal_number`). Reporting those needs the domains registered in
/// the catalogue, which is different machinery — recorded as V48.
fn retype_identifier_columns(view: &str, columns: &mut [ColumnSchema]) {
    // The synth views arrive under their `__spg_` prefix.
    let bare = view.strip_prefix("__spg_").unwrap_or(view);
    let Some((_, names)) = PG_CATALOG_NAME_COLUMNS.iter().find(|(v, _)| *v == bare) else {
        return;
    };
    for c in columns.iter_mut() {
        if names.iter().any(|n| *n == c.name) {
            c.ty = DataType::Name;
        }
    }
}

/// (view, columns) pairs PG types as `name`. See
/// [`retype_identifier_columns`] for why this is a list and not a rule.
static PG_CATALOG_NAME_COLUMNS: &[(&str, &[&str])] = &[
    ("pg_am", &["amname"]),
    ("pg_attribute", &["attname"]),
    ("pg_class", &["relname"]),
    ("pg_collation", &["collname"]),
    ("pg_constraint", &["conname"]),
    ("pg_database", &["datname"]),
    ("pg_enum", &["enumlabel"]),
    ("pg_extension", &["extname"]),
    ("pg_indexes", &["indexname", "schemaname", "tablename", "tablespace"]),
    ("pg_matviews", &["matviewname", "matviewowner", "schemaname", "tablespace"]),
    ("pg_namespace", &["nspname"]),
    ("pg_policies", &["policyname", "schemaname", "tablename"]),
    ("pg_policy", &["polname"]),
    ("pg_proc", &["proname"]),
    ("pg_publication", &["pubname"]),
    ("pg_replication_slots", &["database", "plugin", "slot_name"]),
    ("pg_rewrite", &["rulename"]),
    ("pg_roles", &["rolname"]),
    ("pg_rules", &["rulename", "schemaname", "tablename"]),
    ("pg_stat_database", &["datname"]),
    ("pg_stat_progress_analyze", &["datname"]),
    ("pg_stat_progress_create_index", &["datname"]),
    ("pg_stat_progress_vacuum", &["datname"]),
    ("pg_stat_replication", &["usename"]),
    ("pg_stat_subscription_stats", &["subname"]),
    ("pg_stat_user_functions", &["funcname", "schemaname"]),
    ("pg_stat_user_indexes", &["indexrelname", "relname", "schemaname"]),
    ("pg_stat_user_tables", &["relname", "schemaname"]),
    ("pg_statistic_ext", &["stxname"]),
    ("pg_subscription", &["subname", "subslotname"]),
    ("pg_tables", &["schemaname", "tablename", "tableowner", "tablespace"]),
    ("pg_tablespace", &["spcname"]),
    ("pg_trigger", &["tgname", "tgnewtable", "tgoldtable"]),
    ("pg_type", &["typname"]),
    ("pg_user", &["usename"]),
    ("pg_views", &["schemaname", "viewname", "viewowner"]),
];

/// v7.39 (round 330, V48) — the four domains `information_schema` is built
/// out of, with their base types. PG 18.4 measured:
/// `sql_identifier` is a domain over `name`, `character_data` and
/// `yes_or_no` over `character varying`, `cardinal_number` over `integer`,
/// and every one of them lives in the `information_schema` namespace.
///
/// They are NOT registered as catalog domains: a catalog domain is user
/// data — it serialises into the snapshot and a dump would emit
/// `CREATE DOMAIN` for it. These are built into the server, so they are a
/// table the reflection paths consult instead.
pub(crate) static INFORMATION_SCHEMA_DOMAINS: &[(&str, DataType)] = &[
    ("information_schema.cardinal_number", DataType::Int),
    ("information_schema.character_data", DataType::Text),
    ("information_schema.sql_identifier", DataType::Name),
    ("information_schema.yes_or_no", DataType::Text),
];

/// Is `name` one of them?
#[must_use]
pub(crate) fn is_information_schema_domain(name: &str) -> bool {
    INFORMATION_SCHEMA_DOMAINS.iter().any(|(n, _)| *n == name)
}

/// v7.39 (round 330, V48) — (view, [(column, domain)]) for the
/// information_schema columns PG declares over one of its domains.
/// `pg_typeof` reports the DOMAIN there, not the base type, and the
/// column's storage type follows the domain's base.
static INFORMATION_SCHEMA_DOMAIN_COLUMNS: &[(&str, &[(&str, &str)])] = &[
    (
        "tables",
        &[
            ("table_catalog", "information_schema.sql_identifier"),
            ("table_schema", "information_schema.sql_identifier"),
            ("table_name", "information_schema.sql_identifier"),
            ("table_type", "information_schema.character_data"),
        ],
    ),
    (
        "columns",
        &[
            ("table_catalog", "information_schema.sql_identifier"),
            ("table_schema", "information_schema.sql_identifier"),
            ("table_name", "information_schema.sql_identifier"),
            ("column_name", "information_schema.sql_identifier"),
            ("ordinal_position", "information_schema.cardinal_number"),
            ("is_nullable", "information_schema.yes_or_no"),
            ("data_type", "information_schema.character_data"),
        ],
    ),
    (
        "table_constraints",
        &[
            ("constraint_catalog", "information_schema.sql_identifier"),
            ("constraint_schema", "information_schema.sql_identifier"),
            ("constraint_name", "information_schema.sql_identifier"),
            ("table_catalog", "information_schema.sql_identifier"),
            ("table_schema", "information_schema.sql_identifier"),
            ("table_name", "information_schema.sql_identifier"),
        ],
    ),
    (
        "key_column_usage",
        &[
            ("constraint_name", "information_schema.sql_identifier"),
            ("table_name", "information_schema.sql_identifier"),
            ("column_name", "information_schema.sql_identifier"),
            ("ordinal_position", "information_schema.cardinal_number"),
        ],
    ),
    (
        "schemata",
        &[
            ("catalog_name", "information_schema.sql_identifier"),
            ("schema_name", "information_schema.sql_identifier"),
            ("schema_owner", "information_schema.sql_identifier"),
        ],
    ),
    (
        "views",
        &[
            ("table_catalog", "information_schema.sql_identifier"),
            ("table_schema", "information_schema.sql_identifier"),
            ("table_name", "information_schema.sql_identifier"),
        ],
    ),
];

/// Tag the information_schema columns PG declares over a domain, and give
/// each the domain's base type. Without this `pg_typeof(table_name)`
/// answered the base spelling (`text`) where PG answers
/// `information_schema.sql_identifier`.
fn apply_information_schema_domains(view: &str, columns: &mut [ColumnSchema]) {
    // The synth views arrive as `__spg_info_<name>`.
    let Some(bare) = view.strip_prefix("__spg_info_") else {
        return;
    };
    let Some((_, pairs)) = INFORMATION_SCHEMA_DOMAIN_COLUMNS
        .iter()
        .find(|(v, _)| *v == bare)
    else {
        return;
    };
    for c in columns.iter_mut() {
        let Some((_, domain)) = pairs.iter().find(|(n, _)| *n == c.name) else {
            continue;
        };
        c.user_domain_type = Some(alloc::string::String::from(*domain));
        if let Some((_, base)) = INFORMATION_SCHEMA_DOMAINS
            .iter()
            .find(|(n, _)| n == domain)
        {
            c.ty = *base;
        }
    }
}

pub(crate) fn materialise_meta_view(
    catalog: &mut Catalog,
    name: &str,
    mut columns: Vec<ColumnSchema>,
    rows: Vec<Row<'static>>,
) -> Result<(), EngineError> {
    retype_identifier_columns(name, &mut columns);
    apply_information_schema_domains(name, &mut columns);
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
    if cat.has_view(&tref.name)
        && cat.get(&tref.name).is_none()
        && !into.iter().any(|n| n == &tref.name)
    {
        into.push(tref.name.clone());
    }
}

pub(crate) fn select_references_meta_view(stmt: &SelectStatement) -> bool {
    let mut names = alloc::collections::BTreeSet::new();
    collect_meta_view_names(stmt, &mut names);
    !names.is_empty()
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
    fn walk_table(t: &spg_sql::ast::TableRef, into: &mut alloc::collections::BTreeSet<String>) {
        if is_meta(&t.name) {
            into.insert(t.name.clone());
        }
        // A derived table (`FROM (SELECT …) x`, LATERAL or not) rides
        // the lateral_subquery channel.
        if let Some(sub) = &t.lateral_subquery {
            collect_meta_view_names(sub, into);
        }
    }
    fn walk_expr(e: &Expr, into: &mut alloc::collections::BTreeSet<String>) {
        match e {
            Expr::ScalarSubquery(s) => collect_meta_view_names(s, into),
            Expr::Exists { subquery, .. } => collect_meta_view_names(subquery, into),
            Expr::InSubquery { expr, subquery, .. } => {
                walk_expr(expr, into);
                collect_meta_view_names(subquery, into);
            }
            Expr::Binary { lhs, rhs, .. } => {
                walk_expr(lhs, into);
                walk_expr(rhs, into);
            }
            Expr::Unary { expr, .. } | Expr::Cast { expr, .. } => walk_expr(expr, into),
            Expr::FunctionCall { args, .. } => args.iter().for_each(|a| walk_expr(a, into)),
            Expr::Case {
                operand,
                branches,
                else_branch,
            } => {
                if let Some(o) = operand {
                    walk_expr(o, into);
                }
                for (c, v) in branches {
                    walk_expr(c, into);
                    walk_expr(v, into);
                }
                if let Some(x) = else_branch {
                    walk_expr(x, into);
                }
            }
            Expr::InList { expr, list, .. } => {
                walk_expr(expr, into);
                for it in list {
                    walk_expr(it, into);
                }
            }
            Expr::AnyAll { expr, array, .. } => {
                walk_expr(expr, into);
                walk_expr(array, into);
            }
            Expr::Array(items) => items.iter().for_each(|it| walk_expr(it, into)),
            Expr::ArraySubscript { target, index } => {
                walk_expr(target, into);
                walk_expr(index, into);
            }
            _ => {}
        }
    }
    if let Some(from) = &stmt.from {
        walk_table(&from.primary, into);
        for j in &from.joins {
            walk_table(&j.table, into);
            if let Some(on) = &j.on {
                walk_expr(on, into);
            }
        }
    }
    for item in &stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            walk_expr(expr, into);
        }
    }
    if let Some(w) = &stmt.where_ {
        walk_expr(w, into);
    }
    if let Some(h) = &stmt.having {
        walk_expr(h, into);
    }
    if let Some(gs) = &stmt.group_by {
        for g in gs {
            walk_expr(g, into);
        }
    }
    for o in &stmt.order_by {
        walk_expr(&o.expr, into);
    }
    for (_, peer) in &stmt.unions {
        collect_meta_view_names(peer, into);
    }
    for cte in &stmt.ctes {
        if let Some(s) = cte.body.as_select() {
            collect_meta_view_names(s, into);
        }
    }
}
