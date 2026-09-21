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
pub(crate) fn mysql_data_type_text_of(col: &ColumnSchema) -> alloc::string::String {
    // v7.39.2 — `timestamp` and `datetime` are two types on MySQL and
    // this reported the latter for both; the declared spelling decides.
    if col.mysql_declared_timestamp && matches!(col.ty, DataType::Timestamp | DataType::Timestamptz)
    {
        return alloc::string::String::from("timestamp");
    }
    mysql_data_type_text(col.ty, col.mysql_int_width)
}

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

/// 9.0.0 — a value for one of the catalog columns PostgreSQL declares
/// as `"char"`, its internal single-byte type.
///
/// 28 of them read `text` here, so `pg_typeof(relkind)` answered `text`
/// where PG 18.6 answers `"char"`, and a client that types its result
/// columns saw the wrong OID. An empty string is PG's own value for the
/// columns that carry one (`attidentity` on a plain column), and it is
/// byte 0 there, which prints as nothing.
fn char1(s: &str) -> Value<'static> {
    Value::Char1(s.as_bytes().first().copied().unwrap_or(0))
}

/// 9.0.0 — `information_schema.columns.data_type` and `.attributes
/// .data_type`, and only those two, report every array as the single
/// word `ARRAY`; the element type is left to `udt_name`. Measured on
/// PostgreSQL 18.6, `information_schema.domains.data_type` does NOT —
/// a domain over `int[]` reads `integer[]` there — which is why this
/// is a separate reader and not a change to `pg_data_type_text`.
pub(crate) fn information_schema_data_type(ty: DataType) -> alloc::string::String {
    if crate::conversions::array_element_type(ty).is_some() {
        return alloc::string::String::from("ARRAY");
    }
    match ty {
        // The two catalog vectors and the two catalog array types are
        // arrays to PostgreSQL even though SPG models them as scalars:
        // `pg_index.indkey` and `pg_class.relacl` both read `ARRAY`.
        DataType::Int2Vector
        | DataType::OidVector
        | DataType::AclItemArray
        | DataType::Char1Array
        | DataType::RegTypeArray => alloc::string::String::from("ARRAY"),
        other => pg_data_type_text(other),
    }
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
        DataType::Xid => "xid",
        DataType::Xid8 => "xid8",
        DataType::Tid => "tid",
        DataType::Cid => "cid",
        DataType::RegClass => "regclass",
        DataType::RegType => "regtype",
        DataType::RegProc => "regproc",
        DataType::PgNodeTree => "pg_node_tree",
        DataType::AnyArray => "anyarray",
        DataType::AclItemArray => "aclitem[]",
        DataType::Char1Array => "\"char\"[]",
        DataType::RegTypeArray => "regtype[]",
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
        // v7.39 (round 620) — a `json` column is `json`. `Jsonb` is its own
        // variant right below, so reporting both as jsonb was not a catch-all
        // falling through; it named the wrong type outright.
        DataType::Json => "json",
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
        // 9.0.0 — an array spells its element type. This table used to
        // answer the single word `ARRAY` for every one of them, which is
        // right for exactly two readers (`information_schema.columns`
        // and `.attributes`, which now call `information_schema_data_type`)
        // and wrong for the four others that share it: PostgreSQL 18.6,
        // measured, says `operator does not exist: integer[] + integer`,
        // and `pg_prepared_statements.parameter_types` for `PREPARE
        // p(int[])` reads `{integer[],text}` — both read `ARRAY` here.
        // `dump.rs` had to carry a second copy of this table to undo it.
        DataType::TextArray => "text[]",
        DataType::IntArray => "integer[]",
        DataType::BigIntArray => "bigint[]",
        DataType::SmallIntArray => "smallint[]",
        DataType::FloatArray => "double precision[]",
        DataType::NumericArray => "numeric[]",
        DataType::BoolArray => "boolean[]",
        DataType::DateArray => "date[]",
        DataType::TimestampArray => "timestamp without time zone[]",
        DataType::TimestamptzArray => "timestamp with time zone[]",
        DataType::IntervalArray => "interval[]",
        DataType::UuidArray => "uuid[]",
        DataType::JsonArray => "json[]",
        DataType::JsonbArray => "jsonb[]",
        DataType::BytesArray => "bytea[]",
        DataType::VarcharArray => "character varying[]",
        DataType::CharArray => "character[]",
        DataType::MoneyArray => "money[]",
        DataType::OidArray => "oid[]",
        DataType::RealArray => "real[]",
        DataType::TimeArray => "time without time zone[]",
        DataType::TimeTzArray => "time with time zone[]",
        DataType::InetArray => "inet[]",
        DataType::XmlArray => "xml[]",
        DataType::Vector { .. } => "USER-DEFINED",
        // v7.39.11 — PG's catalog vectors have names, and an error that
        // does not use them ("got USER-DEFINED") tells the reader
        // nothing about what they had.
        DataType::Int2Vector => "int2vector",
        DataType::OidVector => "oidvector",
        // 9.0.0 — `oid` reached the catch-all below, so an `oid` column
        // read `USER-DEFINED` in `information_schema.columns` and the
        // error for `atttypid IN (194)` was `operator does not exist:
        // USER-DEFINED = integer`.
        DataType::Oid => "oid",
        // v7.39.13 — measured on PostgreSQL 18.6, `information_schema
        // .columns.data_type` for a `timetz` column is `time with time
        // zone`; this table answered `USER-DEFINED` while `pg_typeof`
        // and `format_type` over the same column both answered
        // correctly. Three surfaces, one column, two answers.
        DataType::TimeTz => "time with time zone",
        // MySQL's own name for its own type, measured on 9.7.2:
        // `DATA_TYPE` and `COLUMN_TYPE` are both `year`.
        DataType::Year => "year",
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
    // v7.39.2 — the name a MySQL session calls its database. Every row's
    // `TABLE_SCHEMA` carries it, because the canonical MySQL reflection
    // query is `WHERE table_schema = DATABASE()` and this view answered
    // `public` to it — so Django's introspection, Rails' schema dumper
    // and every JDBC tool were told the database has no tables at all.
    schema_name: &str,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    if mysql {
        return mysql_info_columns(cat, schema_name);
    }
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
        // v7.39 (round 675) — the column did not exist, so
        // `SELECT collation_name FROM information_schema.columns` was an
        // "column does not exist" error rather than an answer. PG names the
        // collation for the collatable types and leaves it NULL otherwise.
        ColumnSchema::new("collation_name", DataType::Text, true),
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
    // v7.39 — and the charset a MySQL reflection reads to learn how a
    // column compares. `SELECT CHARACTER_SET_NAME FROM
    // information_schema.COLUMNS` errored with "column does not exist",
    // so an ORM could not tell a case-insensitive column from a binary
    // one. Measured on MySQL 9.7.2: character types report `utf8mb4`,
    // and int, decimal, date, blob and varbinary report NULL.
    //
    // Its partner `collation_name` is NOT appended here: PG's own view
    // already has that column, and a second one of the same name is
    // simply not the one a query resolves to. The first attempt did
    // exactly that, and the tell was a half-right answer — the charset
    // filled in and the collation still NULL. The existing column
    // answers in MySQL's terms on a MySQL session instead.
    let mut rows: Vec<Row<'static>> = Vec::new();
    for tname in cat.visible_relation_keys() {
        // 9.0.0 (C9) — the loop walks KEYS; `tname` is the name a client
        // reads and `nsp` the schema column it belongs in, while every
        // catalog lookup below goes on `key`.
        let key = tname;
        let nsp = alloc::string::String::from(cat.listed_schema(&key));
        let Some(tname) = cat.listed_name(&key).map(alloc::string::String::from) else {
            continue;
        };
        // v7.39 (round 536) — a MATERIALIZED VIEW is not in this view.
        // PG omits them from information_schema entirely (they are not in
        // the SQL standard), and `information_schema.tables` here already
        // did — so SPG listed a relation that had columns and no table
        // row, disagreeing with PG and with itself.
        if cat.materialized_views().contains_key(&key) {
            continue;
        }
        let Some(t) = cat.get(&key) else { continue };
        for (i, col) in t.schema().columns.iter().enumerate() {
            #[allow(clippy::cast_possible_wrap)]
            let ordinal = (i + 1) as i32;
            rows.push(info_column_row(&tname, ordinal, col, None, false));
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
            rows.push(info_column_row(vname, ordinal, col, Some(writable), false));
        }
    }
    // 9.0.0 — the system catalogs' own columns. PostgreSQL 18.6 answers
    // 34 rows for `WHERE table_name='pg_class'` and SPG answered none,
    // so a tool reflecting through `information_schema` was told the
    // catalogs have no columns — or do not exist.
    for (name, _, relkind, _) in CATALOG_RELATIONS {
        let Some(cols) = catalog_relation_columns(name, cat) else {
            continue;
        };
        let updatable = (*relkind == "v").then_some(false);
        for (i, col) in cols.iter().enumerate() {
            #[allow(clippy::cast_possible_wrap)]
            let ordinal = (i + 1) as i32;
            rows.push(info_column_row_in(
                "pg_catalog",
                name,
                ordinal,
                col,
                updatable,
                false,
            ));
        }
    }
    // 9.0.0 (N20) — and `information_schema`'s own columns. PG 18.6
    // answers 696 rows for its own schema and SPG answered none, so a
    // tool reflecting the whole database saw `information_schema` as
    // empty — the same defect as the catalogs above, one schema over.
    for name in crate::info_schema::info_schema_relation_names() {
        let Some(cols) = crate::info_schema::info_schema_columns(name) else {
            continue;
        };
        for (i, col) in cols.iter().enumerate() {
            #[allow(clippy::cast_possible_wrap)]
            let ordinal = (i + 1) as i32;
            rows.push(info_column_row_in(
                "information_schema",
                name,
                ordinal,
                col,
                Some(false),
                false,
            ));
        }
    }
    (schema, rows)
}

/// v7.39.2 — `information_schema.COLUMNS` in MySQL's shape.
///
/// SPG served PostgreSQL's view to the MySQL wire with `column_type`
/// and `character_set_name` bolted onto the end, which matched neither
/// engine: MySQL 9.7.2 has 22 columns in its own order, six of which
/// were simply absent (`CHARACTER_OCTET_LENGTH`, `COLUMN_KEY`,
/// `EXTRA`, `PRIVILEGES`, `COLUMN_COMMENT`, `SRS_ID`), and a client
/// reading by position got PostgreSQL's `udt_name` where MySQL puts
/// `COLUMN_TYPE`.
///
/// The values differ too, not only the shape. Measured against 9.7.2:
/// an INT reports `NUMERIC_PRECISION` 10 (decimal digits) where PG
/// reports 32 (bits); TEXT reports 65535 for both length columns where
/// PG reports NULL; an AUTO_INCREMENT column's `COLUMN_DEFAULT` is NULL
/// with `EXTRA = auto_increment`, where SPG answered PG's
/// `nextval('t_id_seq'::regclass)` — a reflection tool copying that
/// into MySQL DDL writes a statement MySQL cannot parse.
fn mysql_info_columns(cat: &Catalog, schema_name: &str) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("table_catalog", DataType::Text, false),
        ColumnSchema::new("table_schema", DataType::Text, false),
        ColumnSchema::new("table_name", DataType::Text, false),
        ColumnSchema::new("column_name", DataType::Text, false),
        ColumnSchema::new("ordinal_position", DataType::Int, false),
        ColumnSchema::new("column_default", DataType::Text, true),
        ColumnSchema::new("is_nullable", DataType::Text, false),
        ColumnSchema::new("data_type", DataType::Text, false),
        ColumnSchema::new("character_maximum_length", DataType::BigInt, true),
        ColumnSchema::new("character_octet_length", DataType::BigInt, true),
        ColumnSchema::new("numeric_precision", DataType::BigInt, true),
        ColumnSchema::new("numeric_scale", DataType::BigInt, true),
        ColumnSchema::new("datetime_precision", DataType::Int, true),
        ColumnSchema::new("character_set_name", DataType::Text, true),
        ColumnSchema::new("collation_name", DataType::Text, true),
        ColumnSchema::new("column_type", DataType::Text, false),
        ColumnSchema::new("column_key", DataType::Text, false),
        ColumnSchema::new("extra", DataType::Text, false),
        ColumnSchema::new("privileges", DataType::Text, false),
        ColumnSchema::new("column_comment", DataType::Text, false),
        ColumnSchema::new("generation_expression", DataType::Text, false),
        ColumnSchema::new("srs_id", DataType::Int, true),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for tname in cat.visible_relation_keys() {
        // 9.0.0 (C9) — the loop walks KEYS; `tname` is the name a client
        // reads and `nsp` the schema column it belongs in, while every
        // catalog lookup below goes on `key`.
        let key = tname;
        let nsp = alloc::string::String::from(cat.listed_schema(&key));
        let Some(tname) = cat.listed_name(&key).map(alloc::string::String::from) else {
            continue;
        };
        if cat.materialized_views().contains_key(&key) {
            continue;
        }
        let Some(t) = cat.get(&key) else { continue };
        for (i, col) in t.schema().columns.iter().enumerate() {
            #[allow(clippy::cast_possible_wrap)]
            let ordinal = (i + 1) as i32;
            rows.push(mysql_info_column_row(
                schema_name,
                &tname,
                ordinal,
                col,
                mysql_column_key(t, i),
            ));
        }
    }
    for (vname, _) in cat.views_all() {
        let Some(vname) = cat.listed_name(vname) else {
            continue;
        };
        for (i, col) in crate::describe::describe_view_columns(cat, vname)
            .iter()
            .enumerate()
        {
            #[allow(clippy::cast_possible_wrap)]
            let ordinal = (i + 1) as i32;
            rows.push(mysql_info_column_row(schema_name, vname, ordinal, col, ""));
        }
    }
    (schema, rows)
}

/// What MySQL puts in `COLUMN_KEY`, measured on 9.7.2: `PRI` for a
/// primary-key column, `UNI` for the leading column of a unique key that
/// is not the primary one, `MUL` for the leading column of an ordinary
/// index, and the empty string otherwise.
pub(crate) fn mysql_column_key(t: &spg_storage::Table, pos: usize) -> &'static str {
    let ucs = &t.schema().uniqueness_constraints;
    if ucs
        .iter()
        .any(|uc| uc.is_primary_key && uc.columns.contains(&pos))
    {
        return "PRI";
    }
    if ucs
        .iter()
        .any(|uc| !uc.is_primary_key && uc.columns.first() == Some(&pos))
    {
        return "UNI";
    }
    for idx in t.indices() {
        if idx.column_position == pos {
            return if idx.is_unique { "UNI" } else { "MUL" };
        }
    }
    ""
}

/// The two length columns, measured on MySQL 9.7.2. A character type
/// reports its declared limit and that limit in bytes (four per
/// character for utf8mb4); a binary type reports the same number twice;
/// TEXT and BLOB report 65535 for both, where SPG answered NULL.
fn mysql_char_lengths(col: &ColumnSchema) -> (Value<'static>, Value<'static>) {
    let pair = |chars: i64, bytes: i64| (Value::BigInt(chars), Value::BigInt(bytes));
    match col.ty {
        DataType::Varchar(n) | DataType::Char(n) => {
            let n = i64::from(n);
            pair(n, n * 4)
        }
        DataType::Text => pair(65535, 65535),
        DataType::Bytes => pair(65535, 65535),
        _ => (Value::Null, Value::Null),
    }
}

/// `NUMERIC_PRECISION` / `NUMERIC_SCALE` in MySQL's terms. MySQL states
/// an integer's precision in DECIMAL DIGITS (int → 10) where PG states
/// it in bits (int → 32), so the same column has two right answers and
/// each dialect gets its own.
fn mysql_numeric_precision(col: &ColumnSchema) -> (Value<'static>, Value<'static>) {
    use spg_storage::MysqlIntWidth as W;
    let int_digits = |w: Option<W>| -> Option<i64> {
        Some(match w? {
            W::Tiny => 3,
            W::Small => 5,
            W::Medium => 7,
            W::Int => 10,
            // Measured: signed BIGINT reports 19, unsigned 20 — the
            // sign costs a digit.
            W::Big => {
                if col.is_unsigned {
                    20
                } else {
                    19
                }
            }
        })
    };
    if let Some(d) = int_digits(col.mysql_int_width) {
        return (Value::BigInt(d), Value::BigInt(0));
    }
    // v7.39.3 — a declared `(m,d)` is what MySQL reports here
    // (measured: `double(10,2)` reads precision 10, scale 2).
    if let Some((m, d)) = col.mysql_float_md {
        return (Value::BigInt(i64::from(m)), Value::BigInt(i64::from(d)));
    }
    match col.ty {
        DataType::SmallInt => (Value::BigInt(5), Value::BigInt(0)),
        DataType::Int => (Value::BigInt(10), Value::BigInt(0)),
        DataType::BigInt => (
            Value::BigInt(if col.is_unsigned { 20 } else { 19 }),
            Value::BigInt(0),
        ),
        DataType::Bool => (Value::BigInt(3), Value::BigInt(0)),
        DataType::Real => (Value::BigInt(12), Value::Null),
        DataType::Float => (Value::BigInt(22), Value::Null),
        DataType::Numeric { precision: 0, .. } => (Value::Null, Value::Null),
        DataType::Numeric { precision, scale } => (
            Value::BigInt(i64::from(precision)),
            Value::BigInt(i64::from(scale.max(0))),
        ),
        _ => (Value::Null, Value::Null),
    }
}

/// One `information_schema.COLUMNS` row in MySQL's order.
fn mysql_info_column_row(
    schema_name: &str,
    rel: &str,
    ordinal: i32,
    col: &ColumnSchema,
    column_key: &str,
) -> Row<'static> {
    // Measured on 9.7.2: an AUTO_INCREMENT column has NULL here and
    // says so in EXTRA; a CURRENT_TIMESTAMP default is spelled without
    // parentheses and marked DEFAULT_GENERATED; a generated column has
    // NULL here and its source text in GENERATION_EXPRESSION.
    let default_text: Value<'static> = if col.auto_increment || col.generated_stored_expr.is_some()
    {
        Value::Null
    } else if let Some(txt) = &col.default_text {
        Value::text(txt.clone())
    } else {
        Value::Null
    };
    let extra = if col.auto_increment {
        "auto_increment"
    } else if col.generated_stored_expr.is_some() {
        "STORED GENERATED"
    } else if col
        .default_text
        .as_deref()
        .is_some_and(|d| d.eq_ignore_ascii_case("CURRENT_TIMESTAMP"))
    {
        "DEFAULT_GENERATED"
    } else {
        ""
    };
    let (char_max, char_oct) = mysql_char_lengths(col);
    let (num_prec, num_scale) = mysql_numeric_precision(col);
    // Measured: `datetime`/`time`/`timestamp` report their fractional
    // digits, defaulting to 0; SPG reported PG's 6.
    let dt_prec: Value<'static> = match col.ty {
        DataType::Time | DataType::Timestamp | DataType::Timestamptz => Value::Int(0),
        _ => Value::Null,
    };
    Row::new(alloc::vec![
        Value::text("def"),
        Value::text(alloc::string::String::from(schema_name)),
        Value::text(rel.to_string()),
        Value::text(col.name.clone()),
        Value::Int(ordinal),
        default_text,
        Value::text::<&str>(if col.nullable { "YES" } else { "NO" }),
        Value::text(mysql_data_type_text_of(col)),
        char_max,
        char_oct,
        num_prec,
        num_scale,
        dt_prec,
        mysql_column_charset(col),
        mysql_column_collation(col),
        Value::text(crate::show::render_mysql_type(col)),
        Value::text::<&str>(column_key),
        Value::text::<&str>(extra),
        Value::text("select,insert,update,references"),
        Value::text(""),
        match &col.generated_stored_expr {
            Some(e) => Value::text(e.clone()),
            None => Value::text(""),
        },
        Value::Null,
    ])
}

/// v7.39.2 — `information_schema.TABLES` in MySQL's shape (21 columns,
/// measured on 9.7.2). SPG answered PostgreSQL's twelve, so
/// `SELECT ENGINE, TABLE_COLLATION FROM information_schema.TABLES` —
/// what a MySQL reflection reads to learn a table's storage engine and
/// its collation — was a "column does not exist" error, and
/// `WHERE table_schema = DATABASE()` matched nothing.
fn mysql_info_tables(cat: &Catalog, schema_name: &str) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("table_catalog", DataType::Text, false),
        ColumnSchema::new("table_schema", DataType::Text, false),
        ColumnSchema::new("table_name", DataType::Text, false),
        ColumnSchema::new("table_type", DataType::Text, false),
        ColumnSchema::new("engine", DataType::Text, true),
        ColumnSchema::new("version", DataType::Int, true),
        ColumnSchema::new("row_format", DataType::Text, true),
        ColumnSchema::new("table_rows", DataType::BigInt, true),
        ColumnSchema::new("avg_row_length", DataType::BigInt, true),
        ColumnSchema::new("data_length", DataType::BigInt, true),
        ColumnSchema::new("max_data_length", DataType::BigInt, true),
        ColumnSchema::new("index_length", DataType::BigInt, true),
        ColumnSchema::new("data_free", DataType::BigInt, true),
        ColumnSchema::new("auto_increment", DataType::BigInt, true),
        ColumnSchema::new("create_time", DataType::Timestamp, true),
        ColumnSchema::new("update_time", DataType::Timestamp, true),
        ColumnSchema::new("check_time", DataType::Timestamp, true),
        ColumnSchema::new("table_collation", DataType::Text, true),
        ColumnSchema::new("checksum", DataType::BigInt, true),
        ColumnSchema::new("create_options", DataType::Text, true),
        ColumnSchema::new("table_comment", DataType::Text, false),
    ];
    // A VIEW carries NULL through the storage columns on MySQL, which is
    // how a reflection tells the two apart beyond TABLE_TYPE.
    let row = |name: alloc::string::String, is_view: bool| -> Row<'static> {
        let storage = |v: Value<'static>| if is_view { Value::Null } else { v };
        Row::new(alloc::vec![
            Value::text("def"),
            Value::text(alloc::string::String::from(schema_name)),
            Value::text(name),
            Value::text::<&str>(if is_view { "VIEW" } else { "BASE TABLE" }),
            storage(Value::text("InnoDB")),
            storage(Value::Int(10)),
            storage(Value::text("Dynamic")),
            storage(Value::BigInt(0)),
            storage(Value::BigInt(0)),
            storage(Value::BigInt(0)),
            storage(Value::BigInt(0)),
            storage(Value::BigInt(0)),
            storage(Value::BigInt(0)),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            storage(Value::text(alloc::string::String::from(
                crate::collate::MYSQL_DEFAULT_CONNECTION_COLLATION,
            ))),
            Value::Null,
            storage(Value::text("")),
            Value::text(""),
        ])
    };
    let mut rows: Vec<Row<'static>> = Vec::new();
    for tname in cat.visible_relation_keys() {
        // 9.0.0 (C9) — the loop walks KEYS; `tname` is the name a client
        // reads and `nsp` the schema column it belongs in, while every
        // catalog lookup below goes on `key`.
        let key = tname;
        let nsp = alloc::string::String::from(cat.listed_schema(&key));
        let Some(tname) = cat.listed_name(&key).map(alloc::string::String::from) else {
            continue;
        };
        if cat.materialized_views().contains_key(&key) {
            continue;
        }
        rows.push(row(tname.clone(), false));
    }
    for (name, _) in cat.views_all() {
        let Some(name) = cat.listed_name(name) else {
            continue;
        };
        rows.push(row(name.to_string(), true));
    }
    (schema, rows)
}

/// v7.39 (round 268) — one `information_schema.columns` row. Tables and
/// views both come through here so the two can never describe the same
/// column shape differently; `view_updatable` is None for a table and
/// Some(writable) for a view column.
/// What MySQL's `information_schema.COLUMNS` reports for a column's
/// charset and collation.
///
/// v7.39 — character types carry the pair, everything else carries
/// NULL for both (measured on MySQL 9.7.2 across int, decimal, date,
/// blob and varbinary). A `COLLATE` clause is honoured when it names a
/// collation MySQL knows; a PostgreSQL spelling falls back to the
/// default rather than being echoed as a MySQL collation it is not.
fn mysql_is_character_type(col: &ColumnSchema) -> bool {
    matches!(
        col.ty,
        DataType::Text | DataType::Varchar(_) | DataType::Char(_)
    )
}

fn mysql_column_charset(col: &ColumnSchema) -> Value<'static> {
    if !mysql_is_character_type(col) {
        return Value::Null;
    }
    let declared = col
        .collation_name
        .as_deref()
        .and_then(crate::collate::mysql_charset_of_collation);
    Value::text(alloc::string::String::from(declared.unwrap_or("utf8mb4")))
}

/// What MySQL reports in `COLLATION_NAME`: the EFFECTIVE collation, not
/// only a declared one. A `VARCHAR` with no `COLLATE` clause answers
/// `utf8mb4_0900_ai_ci` there, where PG answers NULL for a column at its
/// type's default. The two dialects disagree about the same column name
/// on purpose, so each gets its own answer rather than a second column.
fn mysql_column_collation(col: &ColumnSchema) -> Value<'static> {
    if !mysql_is_character_type(col) {
        return Value::Null;
    }
    match col.collation_name.as_deref() {
        Some(n) if crate::collate::mysql_charset_of_collation(n).is_some() => {
            Value::text(alloc::string::String::from(n))
        }
        _ => Value::text(alloc::string::String::from(
            crate::collate::MYSQL_DEFAULT_CONNECTION_COLLATION,
        )),
    }
}

/// 8.0.3 — an auto-increment column reported the SERIAL way: a
/// `nextval` default and an owned sequence. See `synth_pg_attribute`.
///
/// A column whose default NAMES a sequence (`DEFAULT nextval('s')`) is not
/// one: its default is that text, and the sequence is its own object.
/// 9.0.0 (N23) — `udt_name`: PostgreSQL's internal typname (`int4`,
/// not `integer`). One mapping, because `information_schema.columns`
/// and `column_udt_usage` answer the same question and a second copy
/// is how they would come to disagree.
pub(crate) fn udt_name_for(col: &ColumnSchema) -> &'static str {
    match col.ty {
        DataType::SmallInt => "int2",
        DataType::Int => "int4",
        DataType::BigInt => "int8",
        DataType::Float => "float8",
        DataType::Real => "float4",
        DataType::Bool => "bool",
        DataType::Text => "text",
        DataType::Bytes => "bytea",
        // v7.39 (round 620) — `Jsonb` is its OWN variant beside `Json`,
        // and this table only had the latter, so every JSONB column
        // reported itself as `text` here while `pg_attribute.atttypid`
        // said 3802 right next to it. Two mappings for the same question,
        // disagreeing. And `json` is `json`, not `jsonb`.
        DataType::Json => "json",
        DataType::Jsonb => "jsonb",
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
        // v7.40.0 — `oid[]` became a column type, and a column type
        // that cannot name itself is the defect 7.39.13 closed for
        // `year` and `timetz`. `ARRAY_TYPE_OIDS` already carried
        // (1028, "_oid", 26); these two tables did not.
        DataType::OidArray => "_oid",
        DataType::SmallIntArray => "_int2",
        DataType::FloatArray => "_float8",
        DataType::NumericArray => "_numeric",
        DataType::BoolArray => "_bool",
        DataType::DateArray => "_date",
        DataType::TimestampArray => "_timestamp",
        DataType::TimestamptzArray => "_timestamptz",
        DataType::RealArray => "_float4",
        DataType::TimeArray => "_time",
        DataType::TimeTzArray => "_timetz",
        DataType::InetArray => "_inet",
        DataType::XmlArray => "_xml",
        DataType::UuidArray => "_uuid",
        // v7.39 (round 620) — two more element types whose arrays fell
        // into the text catch-all and named themselves `text`.
        DataType::BytesArray => "_bytea",
        DataType::JsonArray => "_json",
        _ => "text",
    }
}

pub(crate) fn is_serial(col: &ColumnSchema) -> bool {
    col.auto_increment && col.identity.is_none() && col.default_text.is_none()
}

fn info_column_row(
    rel: &str,
    ordinal: i32,
    col: &ColumnSchema,
    view_updatable: Option<bool>,
    mysql: bool,
) -> Row<'static> {
    info_column_row_in("public", rel, ordinal, col, view_updatable, mysql)
}

/// 9.0.0 — the same row, in a named schema. Every caller said `public`
/// because the catalogs were not listed at all.
fn info_column_row_in(
    table_schema: &str,
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
    } else if is_serial(col) {
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
    let udt: &str = udt_name_for(col);
    // v7.39 (round 248) — datetime_precision: PG reports 6 for the
    // microsecond-carrying types and 0 for date.
    let dt_prec: Value<'static> = match col.ty {
        DataType::Date => Value::Int(0),
        DataType::Time | DataType::Timestamp | DataType::Timestamptz | DataType::Interval => {
            Value::Int(6)
        }
        _ => Value::Null,
    };
    Row::new(alloc::vec![
        Value::text("spg"),
        Value::text(alloc::string::String::from(table_schema)),
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
            mysql_data_type_text_of(col)
        } else {
            information_schema_data_type(col.ty)
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
        // v7.39 (round 676) — the EXPLICIT collation, or NULL.
        //
        // Round 675 filled this from the type, mirroring
        // `pg_attribute.attcollation`, and that was wrong: measured on
        // PG18, a plain `TEXT` column reports NULL here while its
        // attcollation is 100. The two columns do not answer the same
        // question — attcollation names the collation in force,
        // information_schema names the one the DDL wrote down.
        // v7.39 (round 679) — report the name the DDL wrote, whatever it
        // was. Round 676 whitelisted C / POSIX / default and answered
        // NULL for anything else, on the reasoning that naming a
        // collation SPG cannot perform would claim too much. Measured
        // against PG, that is backwards: information_schema reports what
        // the DDL DECLARED, and a client reading it back to regenerate
        // DDL needs the name it wrote. What must not overclaim is the
        // BEHAVIOUR, and round 679 makes that explicit instead — a
        // WARNING at CREATE TABLE saying the column is ordered by bytes.
        if mysql {
            mysql_column_collation(col)
        } else {
            match col.collation_name.as_deref() {
                Some(n) => Value::text::<&str>(n),
                None => Value::Null,
            }
        },
        Value::text::<&str>(udt),
        // v7.39 (round 248) — a SERIAL column is NOT identity in PG
        // (is_identity keys off GENERATED … AS IDENTITY).
        // 9.0.0 — and BOTH identity flavours are, each naming itself.
        // Only the ALWAYS one was recorded, so a `GENERATED BY DEFAULT
        // AS IDENTITY` column read `NO` here.
        Value::text::<&str>(if col.identity.is_some() { "YES" } else { "NO" }),
        match col.identity {
            Some(k) => Value::text::<&str>(k.generation()),
            None => Value::Null,
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
/// 9.0.0 — the shape alone, so the catalog can list this
/// relation's columns without building its rows. See `CATALOG_RELATIONS`.
pub(crate) fn pg_prepared_statements_schema() -> Vec<ColumnSchema> {
    alloc::vec![
        ColumnSchema::new("name", DataType::Text, false),
        ColumnSchema::new("statement", DataType::Text, false),
        // 9.0.0 — PostgreSQL 18.6 declares it `regtype[]`.
        ColumnSchema::new("parameter_types", DataType::RegTypeArray, false),
        ColumnSchema::new("from_sql", DataType::Bool, false),
    ]
}

pub(crate) fn synth_pg_prepared_statements(
    prepared: &alloc::collections::BTreeMap<String, crate::PreparedSqlStatement>,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_prepared_statements_schema();
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
    mysql: bool,
    schema_name: &str,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    if mysql {
        return mysql_info_tables(cat, schema_name);
    }
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
    for tname in cat.visible_relation_keys() {
        // 9.0.0 (C9) — the loop walks KEYS; `tname` is the name a client
        // reads and `nsp` the schema column it belongs in, while every
        // catalog lookup below goes on `key`.
        let key = tname;
        let nsp = alloc::string::String::from(cat.listed_schema(&key));
        let Some(tname) = cat.listed_name(&key).map(alloc::string::String::from) else {
            continue;
        };
        // v7.39 (round 267) — a materialized view is backed by a real
        // table in SPG, but PG omits materialized views from this view
        // entirely (they are not in the SQL standard), and reporting one
        // as a BASE TABLE would have a migration tool try to recreate it
        // as a table.
        if cat.materialized_views().contains_key(&key) {
            continue;
        }
        rows.push(Row::new(alloc::vec![
            Value::text("spg"),
            Value::text(nsp.clone()),
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
    // 9.0.0 — the system catalogs. PostgreSQL 18.6 lists them here —
    // `SELECT … WHERE table_name='pg_class'` answers one row there and
    // none here — so a tool that asks `information_schema` what the
    // database holds was told the catalogs do not exist.
    for (name, _, relkind, _) in CATALOG_RELATIONS {
        rows.push(Row::new(alloc::vec![
            Value::text("spg"),
            Value::text("pg_catalog"),
            Value::text((*name).to_string()),
            Value::text::<&str>(if *relkind == "v" {
                "VIEW"
            } else {
                "BASE TABLE"
            }),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::text("NO"),
            Value::text("NO"),
            Value::Null,
        ]));
    }
    // 9.0.0 (N20) — and `information_schema`'s own relations, which it
    // did not list. PostgreSQL 18.6 covers nine schemas here, its own
    // among them; SPG covered `public` and `pg_catalog`, so a tool that
    // asked what `information_schema` holds was told it holds nothing.
    for name in crate::info_schema::info_schema_relation_names() {
        rows.push(Row::new(alloc::vec![
            Value::text("spg"),
            Value::text("information_schema"),
            Value::text(name),
            // Every one of them is a VIEW on PostgreSQL, including the
            // four `_pg_*` helpers.
            Value::text("VIEW"),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::text("NO"),
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
    cat: &Catalog,
    mysql: bool,
    // v7.39.2 — the databases a MySQL session lists, from
    // `Engine::listed_database_names`. In MySQL a schema IS a database
    // and this view lists databases; SPG answered PostgreSQL's three
    // namespaces to both wires, so a MySQL client asking "does database
    // X exist" here got `public`, `pg_catalog` and `information_schema`
    // — and a different answer from `SHOW DATABASES`, which is the same
    // question spelled the other way.
    mysql_databases: &alloc::collections::BTreeSet<alloc::string::String>,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    // v7.39 — the columns a MySQL reflection reads off a schema.
    // PostgreSQL 18.6 has `default_character_set_name` and leaves it
    // NULL (measured), and has no `default_collation_name` at all;
    // MySQL 9.7.2 answers `utf8mb4` and `utf8mb4_0900_ai_ci`. SPG
    // presented PG's shape to both, so a MySQL client asking either got
    // NULL or `column does not exist`.
    //
    // Appended in the MySQL dialect only, which is how `column_type`
    // joins `information_schema.columns`: the view's shape stays PG's on
    // a PG session.
    // v7.39.2 — MySQL's SCHEMATA is its own six columns in its own
    // order (measured on 9.7.2), not PG's seven with a collation
    // appended: a client reading by position found `admin` where the
    // charset belongs, and `DEFAULT_ENCRYPTION` was absent entirely.
    let schema = if mysql {
        alloc::vec![
            ColumnSchema::new("catalog_name", DataType::Text, false),
            ColumnSchema::new("schema_name", DataType::Text, false),
            ColumnSchema::new("default_character_set_name", DataType::Text, true),
            ColumnSchema::new("default_collation_name", DataType::Text, true),
            ColumnSchema::new("sql_path", DataType::Text, true),
            ColumnSchema::new("default_encryption", DataType::Text, false),
        ]
    } else {
        alloc::vec![
            ColumnSchema::new("catalog_name", DataType::Text, false),
            ColumnSchema::new("schema_name", DataType::Text, false),
            ColumnSchema::new("schema_owner", DataType::Text, false),
            ColumnSchema::new("default_character_set_catalog", DataType::Text, true),
            ColumnSchema::new("default_character_set_schema", DataType::Text, true),
            ColumnSchema::new("default_character_set_name", DataType::Text, true),
            ColumnSchema::new("sql_path", DataType::Text, true),
        ]
    };
    // The charset name goes in the column PG already has, so a MySQL
    // client reading `DEFAULT_CHARACTER_SET_NAME` finds it where MySQL
    // puts it.
    let charset = |mysql: bool| -> Value<'static> {
        if mysql {
            Value::text("utf8mb4")
        } else {
            Value::Null
        }
    };
    let coll = |mysql: bool| -> Vec<Value<'static>> {
        if mysql {
            alloc::vec![Value::text(
                crate::collate::MYSQL_DEFAULT_CONNECTION_COLLATION
            )]
        } else {
            Vec::new()
        }
    };
    // v7.39 (round 543) — a row here once carried TEN values against a
    // seven-column schema. Round 543's width check at
    // materialise_meta_view is what found it; nothing had, because a row
    // is an untyped value list. Building the three rows from one shape
    // is why the width can no longer drift per row.
    // v7.39.2 — MySQL's catalog is `def` (measured on 9.7.2); PG's is the
    // database name, which SPG answers as `spg`.
    let catalog_name = if mysql { "def" } else { "spg" };
    let listed: Vec<alloc::string::String> = if mysql {
        mysql_databases.iter().cloned().collect()
    } else {
        // 9.0.0 (C9) — and every schema `CREATE SCHEMA` made, which this
        // view is the SQL-standard way of asking about.
        let mut names: Vec<alloc::string::String> = ["public", "pg_catalog", "information_schema"]
            .into_iter()
            .map(alloc::string::String::from)
            .collect();
        names.extend(cat.user_schemas().iter().cloned());
        names
    };
    let rows: Vec<Row<'static>> = listed
        .into_iter()
        .map(|name| {
            if mysql {
                let mut vals = alloc::vec![Value::text(catalog_name), Value::text(name)];
                vals.push(charset(true));
                vals.extend(coll(true));
                // MySQL's SQL_PATH is always NULL and DEFAULT_ENCRYPTION
                // is `NO` on every schema (measured on 9.7.2).
                vals.push(Value::Null);
                vals.push(Value::text("NO"));
                return Row::new(vals);
            }
            Row::new(alloc::vec![
                Value::text(catalog_name),
                Value::text(name),
                Value::text("admin"),
                Value::Null,
                Value::Null,
                charset(false),
                Value::Null,
            ])
        })
        .collect();
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
                Value::text(crate::eval::functions::pg_viewdef_render_in(
                    &v.body,
                    false,
                    None,
                    Some(cat),
                )),
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
/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows. The rows and the
/// columns come from one place, so `pg_attribute` cannot describe
/// a relation differently from how it answers.
pub(crate) fn pg_stat_progress_vacuum_schema() -> Vec<ColumnSchema> {
    alloc::vec![
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
    ]
}

pub(crate) fn synth_pg_stat_progress_vacuum(
    _cat: &Catalog,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_stat_progress_vacuum_schema();
    let rows: Vec<Row<'static>> = Vec::new();
    (schema, rows)
}

/// v7.37.22 (22.21) — synthesise `pg_catalog.pg_stat_progress_create_index`.
/// PG's per-CREATE-INDEX-in-progress view. SPG's CREATE INDEX
/// is synchronous and finishes inside the wire path; the view
/// shape-stable empties so monitoring queries match PG's
/// "no active build" case.
/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows. The rows and the
/// columns come from one place, so `pg_attribute` cannot describe
/// a relation differently from how it answers.
pub(crate) fn pg_stat_progress_create_index_schema() -> Vec<ColumnSchema> {
    alloc::vec![
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
    ]
}

pub(crate) fn synth_pg_stat_progress_create_index(
    _cat: &Catalog,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_stat_progress_create_index_schema();
    let rows: Vec<Row<'static>> = Vec::new();
    (schema, rows)
}

/// v7.37.22 (22.22) — synthesise
/// `pg_catalog.pg_stat_progress_analyze`. Empty until v7.37.22
/// (22.3) autoanalyze_pass wires per-table progress reporting.
/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows. The rows and the
/// columns come from one place, so `pg_attribute` cannot describe
/// a relation differently from how it answers.
pub(crate) fn pg_stat_progress_analyze_schema() -> Vec<ColumnSchema> {
    alloc::vec![
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
    ]
}

pub(crate) fn synth_pg_stat_progress_analyze(
    _cat: &Catalog,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_stat_progress_analyze_schema();
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
///   * inhseqno (Int) — the parent's position in the CHILD's parent
///     list, NOT the child's index among its siblings. v7.39 (round
///     642) — SPG numbered siblings, so the second partition of one
///     parent read 2 where PG reads 1. Measured both ways: a child of
///     two parents gets 1 and 2 on PG, and two partitions of one parent
///     both get 1. A partition has exactly one parent, so the answer
///     here is always 1.
///   * inhdetachpending (Bool) — false in SPG (DETACH is atomic)
///
/// SPG declarative partitioning (v7.37.6-B + v7.37.16) is the only
/// inheritance source. `CREATE TABLE … INHERITS` is a parse error, not
/// an accept-and-no-op — this comment said otherwise until round 642
/// v7.39 (round 650) — the text-search catalogs SPG can fill honestly.
///
/// `EMPTY_PG_CATALOGS`'s own comment says the `pg_ts_*` family "would
/// NOT be empty — SPG has full-text search. Stubbing those empty would
/// be a lie, so they are recorded as work rather than filled in here."
/// This is that work.
///
/// What SPG actually has is two configurations and two dictionaries —
/// its own error says so: `text search config not implemented: "french"
/// (supported: simple, english)`. PG ships thirty of each; listing
/// thirty here would claim support the engine does not have, the lesson
/// round 639 paid for on `pg_type`. Oids, column names and the
/// `dictinitoption` text are PG18 readings for exactly these rows.
///
/// `pg_ts_config_map` is NOT here, and not in the empty list either: it
/// maps token types to dictionaries, and SPG has no token-type model —
/// the same gap that leaves `ts_token_type` and `ts_debug` unbuilt.
/// Publishing it empty would be the lie the comment warns about; the
/// v7.39 (round 651) — `pg_ts_config_map`, now that there is a
/// token-type model to map FROM.
///
/// Round 650 left this out on purpose: it maps token types to
/// dictionaries and SPG had no token types, so publishing it — empty or
/// full — would have been a claim rather than a fact. The typed
/// tokenizer this round makes it a fact, and the rows are generated
/// from the same `TokenType::dictionary` the indexer calls, so the
/// catalog cannot drift from what the engine does.
///
/// PG maps nineteen of its twenty-three types per configuration; the
/// four it leaves out — blank, tag, protocol, entity — are exactly the
/// ones that produce no lexeme, which is why `<b>x</b>` indexes as `x`.
pub(crate) fn synth_pg_ts_config_map(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    use crate::fts::{TokenType, TsDict};
    let schema = alloc::vec![
        ColumnSchema::new("mapcfg", DataType::Oid, false),
        ColumnSchema::new("maptokentype", DataType::Int, false),
        ColumnSchema::new("mapseqno", DataType::Int, false),
        ColumnSchema::new("mapdict", DataType::Oid, false),
    ];
    const TYPES: &[TokenType] = &[
        TokenType::AsciiWord,
        TokenType::Word,
        TokenType::NumWord,
        TokenType::Email,
        TokenType::Url,
        TokenType::Host,
        TokenType::SFloat,
        TokenType::Version,
        TokenType::HwordNumPart,
        TokenType::HwordPart,
        TokenType::HwordAsciiPart,
        TokenType::Blank,
        TokenType::Tag,
        TokenType::Protocol,
        TokenType::NumHword,
        TokenType::AsciiHword,
        TokenType::Hword,
        TokenType::UrlPath,
        TokenType::File,
        TokenType::Float,
        TokenType::Int,
        TokenType::Uint,
        TokenType::Entity,
    ];
    let mut rows = Vec::new();
    // (config oid, is-english) — PG's own oids, as round 650 published.
    for (cfg_oid, english) in [(3748i64, false), (13248i64, true)] {
        for t in TYPES {
            let Some(dict) = t.dictionary(english) else {
                continue;
            };
            rows.push(Row::new(alloc::vec![
                Value::BigInt(cfg_oid),
                Value::Int(*t as i32),
                Value::Int(1),
                Value::BigInt(match dict {
                    TsDict::Simple => 3765,
                    TsDict::EnglishStem => 13247,
                }),
            ]));
        }
    }
    (schema, rows)
}

/// three of them are one piece of work.
pub(crate) fn synth_pg_ts_config(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("cfgname", DataType::Name, false),
        ColumnSchema::new("cfgnamespace", DataType::Oid, false),
        ColumnSchema::new("cfgowner", DataType::Oid, false),
        ColumnSchema::new("cfgparser", DataType::Oid, false),
    ];
    let rows = alloc::vec![
        Row::new(alloc::vec![
            Value::BigInt(3748),
            Value::text("simple"),
            Value::BigInt(11),
            Value::BigInt(10),
            Value::BigInt(3722),
        ]),
        Row::new(alloc::vec![
            Value::BigInt(13248),
            Value::text("english"),
            Value::BigInt(11),
            Value::BigInt(10),
            Value::BigInt(3722),
        ]),
    ];
    (schema, rows)
}

pub(crate) fn synth_pg_ts_dict(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("dictname", DataType::Name, false),
        ColumnSchema::new("dictnamespace", DataType::Oid, false),
        ColumnSchema::new("dictowner", DataType::Oid, false),
        ColumnSchema::new("dicttemplate", DataType::Oid, false),
        ColumnSchema::new("dictinitoption", DataType::Text, true),
    ];
    let rows = alloc::vec![
        Row::new(alloc::vec![
            Value::BigInt(3765),
            Value::text("simple"),
            Value::BigInt(11),
            Value::BigInt(10),
            Value::BigInt(3727),
            Value::Null,
        ]),
        Row::new(alloc::vec![
            Value::BigInt(13247),
            Value::text("english_stem"),
            Value::BigInt(11),
            Value::BigInt(10),
            Value::BigInt(13234),
            Value::text("language = 'english', stopwords = 'english'"),
        ]),
    ];
    (schema, rows)
}

pub(crate) fn synth_pg_ts_parser(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("prsname", DataType::Name, false),
        ColumnSchema::new("prsnamespace", DataType::Oid, false),
        ColumnSchema::new("prsstart", DataType::RegProc, false),
        ColumnSchema::new("prstoken", DataType::RegProc, false),
        ColumnSchema::new("prsend", DataType::RegProc, false),
        ColumnSchema::new("prsheadline", DataType::RegProc, false),
        ColumnSchema::new("prslextype", DataType::RegProc, false),
    ];
    // One parser, as PG has. The five function oids are 0 for the same
    // reason `pg_type`'s I/O oids are: SPG's parser is built into the
    // engine and is not a catalogued function, so naming one would
    // leave `pg_ts_parser JOIN pg_proc` dangling.
    let rows = alloc::vec![Row::new(alloc::vec![
        Value::BigInt(3722),
        Value::text("default"),
        Value::BigInt(11),
        Value::RegProc(0, "-".into()), // prsstart
        Value::RegProc(0, "-".into()), // prstoken
        Value::RegProc(0, "-".into()), // prsend
        Value::RegProc(0, "-".into()), // prsheadline
        Value::RegProc(0, "-".into()), // prslextype
    ])];
    (schema, rows)
}

pub(crate) fn synth_pg_ts_template(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("tmplname", DataType::Name, false),
        ColumnSchema::new("tmplnamespace", DataType::Oid, false),
        ColumnSchema::new("tmplinit", DataType::RegProc, false),
        ColumnSchema::new("tmpllexize", DataType::RegProc, false),
    ];
    // The two templates the two dictionaries point at. PG also ships
    // synonym, ispell and thesaurus; SPG implements none of them, and
    // `pg_ts_dict.dicttemplate` would have nothing to reference.
    let rows = alloc::vec![
        Row::new(alloc::vec![
            Value::BigInt(3727),
            Value::text("simple"),
            Value::BigInt(11),
            Value::RegProc(0, "-".into()), // tmplinit
            Value::RegProc(0, "-".into()), // tmpllexize
        ]),
        Row::new(alloc::vec![
            Value::BigInt(13234),
            Value::text("snowball"),
            Value::BigInt(11),
            Value::RegProc(0, "-".into()), // tmplinit
            Value::RegProc(0, "-".into()), // tmpllexize
        ]),
    ];
    (schema, rows)
}

/// measured it.
pub(crate) fn synth_pg_inherits(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    use spg_storage::PartitionRole;
    let schema = alloc::vec![
        ColumnSchema::new("inhrelid", DataType::Oid, false),
        ColumnSchema::new("inhparent", DataType::Oid, false),
        ColumnSchema::new("inhseqno", DataType::Int, false),
        ColumnSchema::new("inhdetachpending", DataType::Bool, false),
    ];
    // Build name→OID map matching pg_class's 16384+ band.
    let mut by_name: alloc::collections::BTreeMap<String, i64> =
        alloc::collections::BTreeMap::new();
    let mut oid: i64 = 16384;
    for tname in cat.visible_relation_keys() {
        // 9.0.0 (C9) — the loop walks KEYS; `tname` is the name a client
        // reads and `nsp` the schema column it belongs in, while every
        // catalog lookup below goes on `key`.
        let key = tname;
        let nsp = alloc::string::String::from(cat.listed_schema(&key));
        let Some(tname) = cat.listed_name(&key).map(alloc::string::String::from) else {
            continue;
        };
        by_name.insert(tname.clone(), oid);
        oid = oid.saturating_add(1);
    }
    let mut rows: Vec<Row<'static>> = Vec::new();
    for cname in cat.visible_relation_keys() {
        // 9.0.0 (C9) — the loop walks KEYS; `cname` is the name a client
        // reads and `nsp` the schema column it belongs in, while every
        // catalog lookup below goes on `key`.
        let key = cname;
        let nsp = alloc::string::String::from(cat.listed_schema(&key));
        let Some(cname) = cat.listed_name(&key).map(alloc::string::String::from) else {
            continue;
        };
        let Some(c) = cat.get(&key) else { continue };
        // v7.39 (round 645) — an inheritance child names one or more
        // parents, and a parent's POSITION in that list is exactly what
        // inhseqno means. This is the only shape where it is not 1.
        if let Some(PartitionRole::Inherits { parent_names }) = &c.schema().partition_role {
            let Some(&child_oid) = by_name.get(&key) else {
                continue;
            };
            for (i, pname) in parent_names.iter().enumerate() {
                let Some(&parent_oid) = by_name.get(pname) else {
                    continue;
                };
                rows.push(Row::new(alloc::vec![
                    Value::BigInt(child_oid),
                    Value::BigInt(parent_oid),
                    Value::Int(i as i32 + 1),
                    Value::Bool(false),
                ]));
            }
            continue;
        }
        let parent_name = match &c.schema().partition_role {
            Some(PartitionRole::Range { parent_name, .. })
            | Some(PartitionRole::List { parent_name, .. })
            | Some(PartitionRole::Hash { parent_name, .. })
            | Some(PartitionRole::Default { parent_name }) => parent_name.clone(),
            _ => continue,
        };
        let Some(&child_oid) = by_name.get(&key) else {
            continue;
        };
        let Some(&parent_oid) = by_name.get(&parent_name) else {
            continue;
        };
        rows.push(Row::new(alloc::vec![
            Value::BigInt(child_oid),
            Value::BigInt(parent_oid),
            // Always 1: a partition has exactly one parent, and this
            // column counts parents, not siblings.
            Value::Int(1),
            Value::Bool(false),
        ]));
    }
    // 9.0.0 — an INDEX inherits too. PostgreSQL records a partitioned
    // table's index declaration as the parent of each partition's index
    // here, and `pg_dump` reads exactly this to decide that the parent
    // index is worth writing (`CREATE INDEX … ON ONLY …`) and to emit
    // the `ALTER INDEX … ATTACH PARTITION …` that follows it. Without
    // the rows, `pg_dump` wrote the partitions' indexes alone, so a
    // restore lost the declaration and a partition created afterwards
    // inherited nothing.
    //
    // The pairing is by COLUMNS, not by name: a child's index is named
    // after the child, and the columns are what the template gave it.
    let all = catalog_indexes(cat);
    for parent_idx in all.iter().filter(|ci| ci.only) {
        for child_name in crate::partition::children_of_parent(cat, &parent_idx.table) {
            let Some(child) = cat.get(&child_name) else {
                continue;
            };
            let parent_cols: Vec<&str> = parent_idx
                .columns
                .iter()
                .filter_map(|p| {
                    cat.get(&parent_idx.table)
                        .and_then(|t| t.schema().columns.get(*p))
                        .map(|c| c.name.as_str())
                })
                .collect();
            let Some(child_idx) = all.iter().find(|ci| {
                ci.table == child_name
                    && !ci.only
                    && ci
                        .columns
                        .iter()
                        .filter_map(|p| child.schema().columns.get(*p).map(|c| c.name.as_str()))
                        .eq(parent_cols.iter().copied())
            }) else {
                continue;
            };
            rows.push(Row::new(alloc::vec![
                Value::BigInt(child_idx.oid),
                Value::BigInt(parent_idx.oid),
                Value::Int(1),
                Value::Bool(false),
            ]));
        }
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
/// 7.38.1 S5.1 (pg_dump wall #1) — `pg_catalog.pg_opclass`. pg_dump's
/// first catalog sweep reads every operator class up front
/// (`SELECT tableoid, oid, opcmethod, opcname, opcnamespace, opcowner
/// FROM pg_opclass`) to build its opclass cache. Rows come from
/// SPG's clean-room opclass inventory (opclass.rs, behaviour-aligned
/// against PG18.4 per access method); oids are synthetic and stable
/// (20000 + position — `pg_index.indclass` currently reports 0s, so
/// nothing joins against these yet), `opcmethod` is the real PG am
/// oid via the pg_am mapping, namespace is pg_catalog (11), owner 10.
/// 9.0.0 — PostgreSQL 18.6's own oid for every operator class, read
/// off the live catalog. The rows carried `20000 + position` before,
/// which joined to nothing: a client resolving `pg_index.indclass`
/// against `pg_opclass` found no row, and `indclass` itself was all
/// zeros. A class SPG has and PostgreSQL does not — the pg_trgm and
/// pgvector ones — keeps a synthetic oid.
///
/// `(access method, class name, oid)`, sorted.
const PG_OPCLASS_OIDS: &[(&str, &str, i64)] = &[
    ("brin", "bit_minmax_ops", 10151),
    ("brin", "box_inclusion_ops", 10163),
    ("brin", "bpchar_bloom_ops", 10132),
    ("brin", "bpchar_minmax_ops", 10131),
    ("brin", "bytea_bloom_ops", 10093),
    ("brin", "bytea_minmax_ops", 10092),
    ("brin", "char_bloom_ops", 10095),
    ("brin", "char_minmax_ops", 10094),
    ("brin", "date_bloom_ops", 10138),
    ("brin", "date_minmax_multi_ops", 10137),
    ("brin", "date_minmax_ops", 10136),
    ("brin", "float4_bloom_ops", 10117),
    ("brin", "float4_minmax_multi_ops", 10116),
    ("brin", "float4_minmax_ops", 10115),
    ("brin", "float8_bloom_ops", 10120),
    ("brin", "float8_minmax_multi_ops", 10119),
    ("brin", "float8_minmax_ops", 10118),
    ("brin", "inet_bloom_ops", 10129),
    ("brin", "inet_inclusion_ops", 10130),
    ("brin", "inet_minmax_multi_ops", 10128),
    ("brin", "inet_minmax_ops", 10127),
    ("brin", "int2_bloom_ops", 10103),
    ("brin", "int2_minmax_multi_ops", 10102),
    ("brin", "int2_minmax_ops", 10101),
    ("brin", "int4_bloom_ops", 10106),
    ("brin", "int4_minmax_multi_ops", 10105),
    ("brin", "int4_minmax_ops", 10104),
    ("brin", "int8_bloom_ops", 10100),
    ("brin", "int8_minmax_multi_ops", 10099),
    ("brin", "int8_minmax_ops", 10098),
    ("brin", "interval_bloom_ops", 10147),
    ("brin", "interval_minmax_multi_ops", 10146),
    ("brin", "interval_minmax_ops", 10145),
    ("brin", "macaddr8_bloom_ops", 10126),
    ("brin", "macaddr8_minmax_multi_ops", 10125),
    ("brin", "macaddr8_minmax_ops", 10124),
    ("brin", "macaddr_bloom_ops", 10123),
    ("brin", "macaddr_minmax_multi_ops", 10122),
    ("brin", "macaddr_minmax_ops", 10121),
    ("brin", "name_bloom_ops", 10097),
    ("brin", "name_minmax_ops", 10096),
    ("brin", "numeric_bloom_ops", 10155),
    ("brin", "numeric_minmax_multi_ops", 10154),
    ("brin", "numeric_minmax_ops", 10153),
    ("brin", "oid_bloom_ops", 10111),
    ("brin", "oid_minmax_multi_ops", 10110),
    ("brin", "oid_minmax_ops", 10109),
    ("brin", "pg_lsn_bloom_ops", 10162),
    ("brin", "pg_lsn_minmax_multi_ops", 10161),
    ("brin", "pg_lsn_minmax_ops", 10160),
    ("brin", "range_inclusion_ops", 10159),
    ("brin", "text_bloom_ops", 10108),
    ("brin", "text_minmax_ops", 10107),
    ("brin", "tid_bloom_ops", 10113),
    ("brin", "tid_minmax_multi_ops", 10114),
    ("brin", "tid_minmax_ops", 10112),
    ("brin", "time_bloom_ops", 10135),
    ("brin", "time_minmax_multi_ops", 10134),
    ("brin", "time_minmax_ops", 10133),
    ("brin", "timestamp_bloom_ops", 10141),
    ("brin", "timestamp_minmax_multi_ops", 10140),
    ("brin", "timestamp_minmax_ops", 10139),
    ("brin", "timestamptz_bloom_ops", 10144),
    ("brin", "timestamptz_minmax_multi_ops", 10143),
    ("brin", "timestamptz_minmax_ops", 10142),
    ("brin", "timetz_bloom_ops", 10150),
    ("brin", "timetz_minmax_multi_ops", 10149),
    ("brin", "timetz_minmax_ops", 10148),
    ("brin", "uuid_bloom_ops", 10158),
    ("brin", "uuid_minmax_multi_ops", 10157),
    ("brin", "uuid_minmax_ops", 10156),
    ("brin", "varbit_minmax_ops", 10152),
    ("btree", "array_ops", 10000),
    ("btree", "bit_ops", 10002),
    ("btree", "bool_ops", 10003),
    ("btree", "bpchar_ops", 10004),
    ("btree", "bpchar_pattern_ops", 4219),
    ("btree", "bytea_ops", 10006),
    ("btree", "char_ops", 10007),
    ("btree", "cidr_ops", 10009),
    ("btree", "date_ops", 3122),
    ("btree", "enum_ops", 10069),
    ("btree", "float4_ops", 10012),
    ("btree", "float8_ops", 3123),
    ("btree", "inet_ops", 10015),
    ("btree", "int2_ops", 1979),
    ("btree", "int4_ops", 1978),
    ("btree", "int8_ops", 3124),
    ("btree", "interval_ops", 10022),
    ("btree", "jsonb_ops", 10088),
    ("btree", "macaddr8_ops", 10026),
    ("btree", "macaddr_ops", 10024),
    ("btree", "money_ops", 10047),
    ("btree", "multirange_ops", 10080),
    ("btree", "name_ops", 10028),
    ("btree", "numeric_ops", 3125),
    ("btree", "oid_ops", 1981),
    ("btree", "oidvector_ops", 10032),
    ("btree", "pg_lsn_ops", 10067),
    ("btree", "range_ops", 10076),
    ("btree", "record_image_ops", 10036),
    ("btree", "record_ops", 10034),
    ("btree", "text_ops", 3126),
    ("btree", "text_pattern_ops", 4217),
    ("btree", "tid_ops", 10050),
    ("btree", "time_ops", 10038),
    ("btree", "timestamp_ops", 3128),
    ("btree", "timestamptz_ops", 3127),
    ("btree", "timetz_ops", 10041),
    ("btree", "tsquery_ops", 10074),
    ("btree", "tsvector_ops", 10071),
    ("btree", "uuid_ops", 10065),
    ("btree", "varbit_ops", 10043),
    ("btree", "varchar_ops", 10044),
    ("btree", "varchar_pattern_ops", 4218),
    ("btree", "xid8_ops", 10053),
    ("gin", "array_ops", 10064),
    ("gin", "gin_trgm_ops", 25844),
    ("gin", "jsonb_ops", 10090),
    ("gin", "jsonb_path_ops", 10091),
    ("gin", "tsvector_ops", 10073),
    ("gist", "box_ops", 10060),
    ("gist", "circle_ops", 10063),
    ("gist", "gist_trgm_ops", 25823),
    ("gist", "inet_ops", 10017),
    ("gist", "multirange_ops", 10082),
    ("gist", "point_ops", 10061),
    ("gist", "poly_ops", 10062),
    ("gist", "range_ops", 10078),
    ("gist", "tsquery_ops", 10075),
    ("gist", "tsvector_ops", 10072),
    ("hash", "aclitem_ops", 10059),
    ("hash", "array_ops", 10001),
    ("hash", "bool_ops", 10048),
    ("hash", "bpchar_ops", 10005),
    ("hash", "bpchar_pattern_ops", 10058),
    ("hash", "bytea_ops", 10049),
    ("hash", "char_ops", 10008),
    ("hash", "cid_ops", 10054),
    ("hash", "cidr_ops", 10010),
    ("hash", "date_ops", 10011),
    ("hash", "enum_ops", 10070),
    ("hash", "float4_ops", 10013),
    ("hash", "float8_ops", 10014),
    ("hash", "inet_ops", 10016),
    ("hash", "int2_ops", 10019),
    ("hash", "int4_ops", 10020),
    ("hash", "int8_ops", 10021),
    ("hash", "interval_ops", 10023),
    ("hash", "jsonb_ops", 10089),
    ("hash", "macaddr8_ops", 10027),
    ("hash", "macaddr_ops", 10025),
    ("hash", "multirange_ops", 10081),
    ("hash", "name_ops", 10029),
    ("hash", "numeric_ops", 10030),
    ("hash", "oid_ops", 10031),
    ("hash", "oidvector_ops", 10033),
    ("hash", "pg_lsn_ops", 10068),
    ("hash", "range_ops", 10077),
    ("hash", "record_ops", 10035),
    ("hash", "text_ops", 10037),
    ("hash", "text_pattern_ops", 10056),
    ("hash", "tid_ops", 10055),
    ("hash", "time_ops", 10039),
    ("hash", "timestamp_ops", 10046),
    ("hash", "timestamptz_ops", 10040),
    ("hash", "timetz_ops", 10042),
    ("hash", "uuid_ops", 10066),
    ("hash", "varchar_ops", 10045),
    ("hash", "varchar_pattern_ops", 10057),
    ("hash", "xid8_ops", 10052),
    ("hash", "xid_ops", 10051),
    ("spgist", "box_ops", 10083),
    ("spgist", "inet_ops", 10018),
    ("spgist", "kd_point_ops", 10085),
    ("spgist", "poly_ops", 10087),
    ("spgist", "quad_point_ops", 10084),
    ("spgist", "range_ops", 10079),
    ("spgist", "text_ops", 10086),
];

/// 9.0.0 — the DEFAULT operator class for `(access method, type oid)`,
/// read off PostgreSQL 18.6. `pg_index.indclass` names one per key
/// part; the part with no `USING … (col opclass)` gets this one.
/// An array type takes the `anyarray` row (2277), which is what PG
/// answers for `USING gin (int_array_column)`.
const PG_DEFAULT_OPCLASS_FOR_TYPE: &[(&str, i64, i64)] = &[
    ("brin", 17, 10092),
    ("brin", 18, 10094),
    ("brin", 19, 10096),
    ("brin", 20, 10098),
    ("brin", 21, 10101),
    ("brin", 23, 10104),
    ("brin", 25, 10107),
    ("brin", 26, 10109),
    ("brin", 27, 10112),
    ("brin", 603, 10163),
    ("brin", 700, 10115),
    ("brin", 701, 10118),
    ("brin", 774, 10124),
    ("brin", 829, 10121),
    ("brin", 869, 10130),
    ("brin", 1042, 10131),
    ("brin", 1082, 10136),
    ("brin", 1083, 10133),
    ("brin", 1114, 10139),
    ("brin", 1184, 10142),
    ("brin", 1186, 10145),
    ("brin", 1266, 10148),
    ("brin", 1560, 10151),
    ("brin", 1562, 10152),
    ("brin", 1700, 10153),
    ("brin", 2950, 10156),
    ("brin", 3220, 10160),
    ("brin", 3831, 10159),
    ("btree", 16, 10003),
    ("btree", 17, 10006),
    ("btree", 18, 10007),
    ("btree", 19, 10028),
    ("btree", 20, 3124),
    ("btree", 21, 1979),
    ("btree", 23, 1978),
    ("btree", 25, 3126),
    ("btree", 26, 1981),
    ("btree", 27, 10050),
    ("btree", 30, 10032),
    ("btree", 700, 10012),
    ("btree", 701, 3123),
    ("btree", 774, 10026),
    ("btree", 790, 10047),
    ("btree", 829, 10024),
    ("btree", 869, 10015),
    ("btree", 1042, 10004),
    ("btree", 1082, 3122),
    ("btree", 1083, 10038),
    ("btree", 1114, 3128),
    ("btree", 1184, 3127),
    ("btree", 1186, 10022),
    ("btree", 1266, 10041),
    ("btree", 1560, 10002),
    ("btree", 1562, 10043),
    ("btree", 1700, 3125),
    ("btree", 2249, 10034),
    ("btree", 2277, 10000),
    ("btree", 2950, 10065),
    ("btree", 3220, 10067),
    ("btree", 3500, 10069),
    ("btree", 3614, 10071),
    ("btree", 3615, 10074),
    ("btree", 3802, 10088),
    ("btree", 3831, 10076),
    ("btree", 4537, 10080),
    ("btree", 5069, 10053),
    ("gin", 2277, 10064),
    ("gin", 3614, 10073),
    ("gin", 3802, 10090),
    ("gist", 600, 10061),
    ("gist", 603, 10060),
    ("gist", 604, 10062),
    ("gist", 718, 10063),
    ("gist", 3614, 10072),
    ("gist", 3615, 10075),
    ("gist", 3831, 10078),
    ("gist", 4537, 10082),
    ("hash", 16, 10048),
    ("hash", 17, 10049),
    ("hash", 18, 10008),
    ("hash", 19, 10029),
    ("hash", 20, 10021),
    ("hash", 21, 10019),
    ("hash", 23, 10020),
    ("hash", 25, 10037),
    ("hash", 26, 10031),
    ("hash", 27, 10055),
    ("hash", 28, 10051),
    ("hash", 29, 10054),
    ("hash", 30, 10033),
    ("hash", 700, 10013),
    ("hash", 701, 10014),
    ("hash", 774, 10027),
    ("hash", 829, 10025),
    ("hash", 869, 10016),
    ("hash", 1033, 10059),
    ("hash", 1042, 10005),
    ("hash", 1082, 10011),
    ("hash", 1083, 10039),
    ("hash", 1114, 10046),
    ("hash", 1184, 10040),
    ("hash", 1186, 10023),
    ("hash", 1266, 10042),
    ("hash", 1700, 10030),
    ("hash", 2249, 10035),
    ("hash", 2277, 10001),
    ("hash", 2950, 10066),
    ("hash", 3220, 10068),
    ("hash", 3500, 10070),
    ("hash", 3802, 10089),
    ("hash", 3831, 10077),
    ("hash", 4537, 10081),
    ("hash", 5069, 10052),
    ("spgist", 25, 10086),
    ("spgist", 600, 10084),
    ("spgist", 603, 10083),
    ("spgist", 604, 10087),
    ("spgist", 869, 10018),
    ("spgist", 3831, 10079),
];

/// The oid PostgreSQL gives this operator class, or `None` when it is one
/// SPG has and PostgreSQL does not.
fn pg_opclass_oid(am: &str, name: &str) -> Option<i64> {
    PG_OPCLASS_OIDS
        .iter()
        .find(|(a, n, _)| *a == am && *n == name)
        .map(|(_, _, oid)| *oid)
}

/// The oid of the operator class PostgreSQL picks for a key part of this
/// type under this access method, when the index names none.
fn pg_default_opclass_oid(am: &str, type_oid: i64) -> Option<i64> {
    PG_DEFAULT_OPCLASS_FOR_TYPE
        .iter()
        .find(|(a, t, _)| *a == am && *t == type_oid)
        .map(|(_, _, oid)| *oid)
}

pub(crate) fn synth_pg_opclass(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("opcmethod", DataType::Oid, false),
        ColumnSchema::new("opcname", DataType::Name, false),
        ColumnSchema::new("opcnamespace", DataType::Oid, false),
        ColumnSchema::new("opcowner", DataType::Oid, false),
        ColumnSchema::new("opcfamily", DataType::Oid, false),
        ColumnSchema::new("opcintype", DataType::Oid, false),
        ColumnSchema::new("opcdefault", DataType::Bool, false),
        ColumnSchema::new("opckeytype", DataType::Oid, false),
    ];
    let am_oid = |am: &str| -> i64 {
        match am {
            "btree" => 403,
            "hash" => 405,
            "gist" => 783,
            "gin" => 2742,
            "spgist" => 4000,
            "brin" => 3580,
            // pgvector AMs carry extension-local oids in PG; a stable
            // synthetic pair keeps the join surface consistent.
            "hnsw" => 20403,
            "ivfflat" => 20404,
            _ => 0,
        }
    };
    let mut rows: Vec<Row<'static>> = Vec::new();
    for (i, (am, name)) in crate::opclass::all_opclasses().enumerate() {
        // 9.0.0 — PostgreSQL's oid when it has this class, so
        // `pg_index.indclass` joins; the synthetic one only for a class
        // PostgreSQL does not carry.
        let oid = pg_opclass_oid(am, name).unwrap_or(20000 + i as i64);
        rows.push(Row::new(alloc::vec![
            Value::BigInt(oid),
            Value::BigInt(am_oid(am)),
            Value::text(name),
            Value::BigInt(11),
            Value::BigInt(10),
            Value::BigInt(oid),
            Value::BigInt(0),
            Value::Bool(false),
            Value::BigInt(0),
        ]));
    }
    (schema, rows)
}

/// 7.38.1 S5.1 (pg_dump wall #2) — `pg_catalog.pg_opfamily`. Paired
/// 1:1 with the pg_opclass synthesis above (`opcfamily == oid`), same
/// oid band, so pg_dump's family cache joins cleanly against the
/// classes.
pub(crate) fn synth_pg_opfamily(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("opfmethod", DataType::Oid, false),
        ColumnSchema::new("opfname", DataType::Name, false),
        ColumnSchema::new("opfnamespace", DataType::Oid, false),
        ColumnSchema::new("opfowner", DataType::Oid, false),
    ];
    let (oc_schema, oc_rows) = synth_pg_opclass(cat);
    let _ = oc_schema;
    let mut rows: Vec<Row<'static>> = Vec::new();
    for r in oc_rows {
        // oid, opcmethod, opcname mirror into the family row.
        rows.push(Row::new(alloc::vec![
            r.values[0].clone(),
            r.values[1].clone(),
            r.values[2].clone(),
            Value::BigInt(11),
            Value::BigInt(10),
        ]));
    }
    (schema, rows)
}

/// 7.38.1 S5.1 (pg_dump walls) — `pg_amop` / `pg_amproc`: the
/// operator-class member catalogs. Shape-stable EMPTY: SPG's operator
/// resolution is engine-internal, and pg_dump only joins these against
/// pg_depend (also empty) to find extension-owned members.
pub(crate) fn synth_pg_amop(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("amopfamily", DataType::Oid, false),
        ColumnSchema::new("amoplefttype", DataType::Oid, false),
        ColumnSchema::new("amoprighttype", DataType::Oid, false),
        ColumnSchema::new("amopstrategy", DataType::SmallInt, false),
        ColumnSchema::new("amoppurpose", DataType::Char1, false),
        ColumnSchema::new("amopopr", DataType::Oid, false),
        ColumnSchema::new("amopmethod", DataType::Oid, false),
        ColumnSchema::new("amopsortfamily", DataType::Oid, false),
    ];
    (schema, Vec::new())
}

pub(crate) fn synth_pg_amproc(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("amprocfamily", DataType::Oid, false),
        ColumnSchema::new("amproclefttype", DataType::Oid, false),
        ColumnSchema::new("amprocrighttype", DataType::Oid, false),
        ColumnSchema::new("amprocnum", DataType::SmallInt, false),
        ColumnSchema::new("amproc", DataType::RegProc, false),
    ];
    (schema, Vec::new())
}

pub(crate) fn synth_pg_depend(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("classid", DataType::Oid, false),
        ColumnSchema::new("objid", DataType::Oid, false),
        ColumnSchema::new("objsubid", DataType::Int, false),
        ColumnSchema::new("refclassid", DataType::Oid, false),
        ColumnSchema::new("refobjid", DataType::Oid, false),
        ColumnSchema::new("refobjsubid", DataType::Int, false),
        ColumnSchema::new("deptype", DataType::Char1, false),
    ];
    // 7.38.1 S5.2 — views and materialized views DEPEND on the
    // relations their body reads, and pg_dump orders its output by
    // walking exactly these edges: with an empty pg_depend it sorted
    // mv1 before the table it selects from and the restore died on
    // "relation does not exist". classid/refclassid are pg_class
    // (1259), deptype 'n' (normal), one edge per body relation whose
    // name resolves. The body is SPG's own stored SQL; the FROM list
    // comes from the parser, not from guessing at text.
    let mut rows: Vec<Row<'static>> = Vec::new();
    let mut push_edges = |body: &str, self_oid: i64, rows: &mut Vec<Row<'static>>| {
        let Ok(spg_sql::ast::Statement::Select(sel)) =
            spg_sql::parser::parse_statement_with(body, spg_sql::lexer::Dialect::PG)
        else {
            return;
        };
        for tname in crate::transaction::read_tables_of(&spg_sql::ast::Statement::Select(sel)) {
            if let Some(ref_oid) = relation_oid(cat, &tname) {
                rows.push(Row::new(alloc::vec![
                    Value::BigInt(1259),
                    Value::BigInt(self_oid),
                    Value::Int(0),
                    Value::BigInt(1259),
                    Value::BigInt(ref_oid),
                    Value::Int(0),
                    char1("n"), // deptype
                ]));
            }
        }
    };
    for (vname, def) in cat.views_all() {
        let Some(listed) = cat.listed_name(vname) else {
            continue;
        };
        if let Some(self_oid) = relation_oid(cat, listed) {
            push_edges(&def.body, self_oid, &mut rows);
        }
    }
    for (mname, body) in cat.materialized_views() {
        if let Some(self_oid) = relation_oid(cat, mname) {
            push_edges(body, self_oid, &mut rows);
        }
    }
    // 8.0.3 — a sequence OWNED BY a column depends on that column:
    // automatically ('a') for a SERIAL column, internally ('i') for an
    // identity one. Measured on PG 18.6. `pg_dump` reads exactly these
    // edges to write `ALTER SEQUENCE … OWNED BY` or the identity clause;
    // with none, a serial column's sequence restored unattached.
    for (stored, def) in crate::sequence::catalog_sequences(cat) {
        let Some((table, column)) = &def.owned_by else {
            continue;
        };
        let (Some(name), Some(t)) = (cat.listed_name(&stored), cat.get(table)) else {
            continue;
        };
        let Some(pos) = t.schema().columns.iter().position(|c| c.name == *column) else {
            continue;
        };
        let (Some(seq_oid), Some(table_oid)) = (relation_oid(cat, name), relation_oid(cat, table))
        else {
            continue;
        };
        let attnum = i32::try_from(pos + 1).unwrap_or(i32::MAX);
        // An identity column's sequence is INTERNAL to it ('i'); a
        // serial's is merely AUTO ('a'), which is what lets a serial's
        // sequence be dropped on its own.
        let deptype = if t.schema().columns[pos].identity.is_some() {
            "i"
        } else {
            "a"
        };
        rows.push(dep_row(
            (1259, seq_oid, 0),
            (1259, table_oid, attnum),
            deptype,
        ));
    }
    // 8.0.3 — every default belongs to its column ('a'), and a partition to
    // its parent ('a'), as PG 18.6 records both. The partition edge is what
    // orders the parent's CREATE TABLE ahead of its partitions in a dump.
    for (row_oid, table_oid, attnum, text) in attrdef_rows(cat) {
        rows.push(dep_row((2604, row_oid, 0), (1259, table_oid, attnum), "a"));
        // A default that names a sequence depends on it, whatever made
        // the sequence — which is what orders `CREATE SEQUENCE` ahead of
        // the table in a dump.
        if let Some(seq_oid) = nextval_target(&text).and_then(|s| relation_oid(cat, &s)) {
            rows.push(dep_row((2604, row_oid, 0), (1259, seq_oid, 0), "n"));
        }
    }
    for tname in cat.visible_relation_keys() {
        // 9.0.0 (C9) — the loop walks KEYS; `tname` is the name a client
        // reads and `nsp` the schema column it belongs in, while every
        // catalog lookup below goes on `key`.
        let key = tname;
        let nsp = alloc::string::String::from(cat.listed_schema(&key));
        let Some(tname) = cat.listed_name(&key).map(alloc::string::String::from) else {
            continue;
        };
        let Some(t) = cat.get(&key) else { continue };
        use spg_storage::PartitionRole as R;
        let parent = match t.schema().partition_role.as_ref() {
            Some(
                R::Range { parent_name, .. }
                | R::List { parent_name, .. }
                | R::Hash { parent_name, .. }
                | R::Default { parent_name, .. },
            ) => parent_name,
            _ => continue,
        };
        if let (Some(child), Some(parent)) = (relation_oid(cat, &tname), relation_oid(cat, parent))
        {
            rows.push(dep_row((1259, child, 0), (1259, parent, 0), "a"));
        }
    }
    (schema, rows)
}

/// One pg_depend row: `(classid, objid, objsubid)` depends on
/// `(refclassid, refobjid, refobjsubid)`.
fn dep_row(
    obj: (i64, i64, i32),
    referenced: (i64, i64, i32),
    deptype: &'static str,
) -> Row<'static> {
    Row::new(alloc::vec![
        Value::BigInt(obj.0),
        Value::BigInt(obj.1),
        Value::Int(obj.2),
        Value::BigInt(referenced.0),
        Value::BigInt(referenced.1),
        Value::Int(referenced.2),
        char1(deptype),
    ])
}

/// The oid `pg_attrdef` gives a column's default.
fn attrdef_oid(table_oid: i64, attnum: i32) -> i64 {
    table_oid
        .saturating_mul(1000)
        .saturating_add(i64::from(attnum))
}

/// `(attrdef oid, table oid, attnum, default text)` for every column
/// default, in the numbering `synth_pg_attrdef` publishes.
fn attrdef_rows(cat: &Catalog) -> Vec<(i64, i64, i32, String)> {
    let (_, rows) = synth_pg_attrdef(cat);
    rows.iter()
        .filter_map(
            |r| match (&r.values[0], &r.values[1], &r.values[2], &r.values[3]) {
                (Value::BigInt(o), Value::BigInt(t), Value::SmallInt(n), Value::Text(x)) => {
                    Some((*o, *t, i32::from(*n), x.to_string()))
                }
                _ => None,
            },
        )
        .collect()
}

/// The sequence a `nextval('s')` default reads, schema prefix dropped.
fn nextval_target(default_text: &str) -> Option<String> {
    let Ok(spg_sql::ast::Expr::FunctionCall { name, args, .. }) =
        spg_sql::parser::parse_expression(default_text)
    else {
        return None;
    };
    if !name.eq_ignore_ascii_case("nextval") {
        return None;
    }
    let lit = match args.first()? {
        spg_sql::ast::Expr::Literal(spg_sql::ast::Literal::String(s)) => s,
        spg_sql::ast::Expr::Cast { expr, .. } => match expr.as_ref() {
            spg_sql::ast::Expr::Literal(spg_sql::ast::Literal::String(s)) => s,
            _ => return None,
        },
        _ => return None,
    };
    Some(String::from(lit.strip_prefix("public.").unwrap_or(lit)))
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
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("adrelid", DataType::Oid, false),
        ColumnSchema::new("adnum", DataType::SmallInt, false),
        ColumnSchema::new("adbin", DataType::PgNodeTree, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for tname in cat.visible_relation_keys() {
        // 9.0.0 (C9) — the loop walks KEYS; `tname` is the name a client
        // reads and `nsp` the schema column it belongs in, while every
        // catalog lookup below goes on `key`.
        let key = tname;
        let nsp = alloc::string::String::from(cat.listed_schema(&key));
        let Some(tname) = cat.listed_name(&key).map(alloc::string::String::from) else {
            continue;
        };
        // 8.0.3 — pg_class's oid for the table. A running count over the
        // visible names drifted from it past any other session's
        // temporary table, which pg_class still numbers.
        let (Some(t), Some(table_oid)) = (cat.get(&key), relation_oid(cat, &key)) else {
            continue;
        };
        for (i, col) in t.schema().columns.iter().enumerate() {
            // 8.0.3 — a SERIAL column's default is its sequence's nextval,
            // as PG stores it; it had no row, so `pg_dump` never wrote the
            // `SET DEFAULT` and the column restored without one.
            let serial_default;
            let txt = match &col.default_text {
                Some(txt) => txt,
                None if is_serial(col) => {
                    serial_default =
                        alloc::format!("nextval('{tname}_{}_seq'::regclass)", col.name);
                    &serial_default
                }
                None => continue,
            };
            #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
            let adnum = (i + 1) as i16;
            // Synthetic row OID: table OID × 1000 + column position. Distinct
            // per default, in a band that won't collide with table OIDs.
            let row_oid = attrdef_oid(table_oid, i32::from(adnum));
            rows.push(Row::new(alloc::vec![
                Value::BigInt(row_oid),
                Value::BigInt(table_oid),
                Value::SmallInt(adnum),
                Value::text(txt.clone()),
            ]));
        }
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
pub(crate) fn synth_pg_policy(
    cat: &Catalog,
    roles: &crate::role_directory::RoleDirectory,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("polname", DataType::Name, false),
        ColumnSchema::new("polrelid", DataType::Oid, false),
        ColumnSchema::new("polcmd", DataType::Char1, false),
        ColumnSchema::new("polpermissive", DataType::Bool, false),
        ColumnSchema::new("polroles", DataType::OidArray, false),
        ColumnSchema::new("polqual", DataType::PgNodeTree, true),
        ColumnSchema::new("polwithcheck", DataType::PgNodeTree, true),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    let mut table_oid: i64 = 16384;
    for tname in cat.visible_relation_keys() {
        // 9.0.0 (C9) — the loop walks KEYS; `tname` is the name a client
        // reads and `nsp` the schema column it belongs in, while every
        // catalog lookup below goes on `key`.
        let key = tname;
        let nsp = alloc::string::String::from(cat.listed_schema(&key));
        let Some(tname) = cat.listed_name(&key).map(alloc::string::String::from) else {
            continue;
        };
        let Some(t) = cat.get(&key) else {
            table_oid = table_oid.saturating_add(1);
            continue;
        };
        for (i, p) in t.schema().policies.iter().enumerate() {
            #[allow(clippy::cast_possible_wrap)]
            let row_oid = table_oid.saturating_mul(1000).saturating_add(i as i64 + 1);
            // 9.0.0 — `oid[]`, as PostgreSQL declares it, and the
            // grantee's own oid. Every entry used to be 0, which is
            // PUBLIC: `CREATE POLICY … TO alice` read `{0}` and said
            // the policy applies to everyone. `pg_policies.roles`, the
            // view over the same fact, named `alice` correctly — two
            // surfaces, one question, and only one of them right.
            let role_oids: Vec<Option<i64>> = if p.roles.is_empty() {
                alloc::vec![Some(0)]
            } else {
                p.roles
                    .iter()
                    .map(|r| {
                        // PUBLIC is 0 on PostgreSQL, whichever way it
                        // was spelled.
                        Some(if r.eq_ignore_ascii_case("public") {
                            0
                        } else {
                            roles.owner_oid(Some(r.as_str()))
                        })
                    })
                    .collect()
            };
            rows.push(Row::new(alloc::vec![
                Value::BigInt(row_oid),
                Value::text(p.name.clone()),
                Value::BigInt(table_oid),
                Value::Char1(p.cmd.as_pg_char() as u8), // polcmd
                Value::Bool(p.permissive),
                Value::BigIntArray(role_oids), // polroles
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
/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows. The rows and the
/// columns come from one place, so `pg_attribute` cannot describe
/// a relation differently from how it answers.
pub(crate) fn pg_policies_schema() -> Vec<ColumnSchema> {
    alloc::vec![
        ColumnSchema::new("schemaname", DataType::Name, false),
        ColumnSchema::new("tablename", DataType::Name, false),
        ColumnSchema::new("policyname", DataType::Text, false),
        ColumnSchema::new("permissive", DataType::Text, false),
        ColumnSchema::new("roles", DataType::Text, false),
        ColumnSchema::new("cmd", DataType::Text, false),
        ColumnSchema::new("qual", DataType::Text, true),
        ColumnSchema::new("with_check", DataType::Text, true),
    ]
}

pub(crate) fn synth_pg_policies(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_policies_schema();
    let mut rows: Vec<Row<'static>> = Vec::new();
    for tname in cat.visible_relation_keys() {
        // 9.0.0 (C9) — the loop walks KEYS; `tname` is the name a client
        // reads and `nsp` the schema column it belongs in, while every
        // catalog lookup below goes on `key`.
        let key = tname;
        let nsp = alloc::string::String::from(cat.listed_schema(&key));
        let Some(tname) = cat.listed_name(&key).map(alloc::string::String::from) else {
            continue;
        };
        let Some(t) = cat.get(&key) else { continue };
        for p in &t.schema().policies {
            let roles = if p.roles.is_empty() {
                alloc::string::String::from("{public}")
            } else {
                alloc::format!("{{{}}}", p.roles.join(","))
            };
            rows.push(Row::new(alloc::vec![
                Value::text(nsp.clone()),
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
        ColumnSchema::new("loid", DataType::Oid, false),
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
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("lomowner", DataType::Oid, false),
        ColumnSchema::new("lomacl", DataType::AclItemArray, true),
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
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("stxrelid", DataType::Oid, false),
        ColumnSchema::new("stxname", DataType::Name, false),
        ColumnSchema::new("stxnamespace", DataType::Oid, false),
        ColumnSchema::new("stxowner", DataType::Oid, false),
        // v7.39 (round 543) — PG's order is stxkeys, stxstattarget,
        // stxkind, stxexprs; SPG had the first two swapped and the
        // other two missing.
        ColumnSchema::new("stxkeys", DataType::Int2Vector, false),
        ColumnSchema::new("stxstattarget", DataType::SmallInt, true),
        ColumnSchema::new("stxkind", DataType::Char1Array, false),
        ColumnSchema::new("stxexprs", DataType::PgNodeTree, true),
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
                // 9.0.0 — `int2vector`, as PostgreSQL declares it.
                Value::Int2Vector(
                    st.columns
                        .iter()
                        .filter_map(|c| c.parse::<i16>().ok())
                        .collect(),
                ),
                Value::Null, // stxstattarget — the default target
                Value::text(alloc::format!("{{{}}}", st.kinds.join(","))),
                Value::Null, // stxexprs — SPG has no expression statistics
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
/// v7.38.18 — `pg_catalog.pg_stats`, the readable view over
/// [`synth_pg_statistic`] and the one a person actually types to ask
/// whether `ANALYZE` did anything.
///
/// PostgreSQL 18.4's column list, in its order. The bounds and
/// most-common arrays are NULL until SPG carries them here — the
/// three columns that answer the usual question (`attname`,
/// `null_frac`, `n_distinct`) are real, and a NULL says "not
/// modelled" rather than "zero", which is the difference between
/// admitting a gap and reporting a wrong number.
pub(crate) fn synth_pg_stats(
    cat: &Catalog,
    stats: &crate::statistics::Statistics,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("schemaname", DataType::Name, false),
        ColumnSchema::new("tablename", DataType::Name, false),
        ColumnSchema::new("attname", DataType::Name, false),
        ColumnSchema::new("inherited", DataType::Bool, false),
        ColumnSchema::new("null_frac", DataType::Real, false),
        ColumnSchema::new("avg_width", DataType::Int, false),
        ColumnSchema::new("n_distinct", DataType::Real, false),
        ColumnSchema::new("most_common_vals", DataType::AnyArray, true),
        ColumnSchema::new("most_common_freqs", DataType::RealArray, true),
        ColumnSchema::new("histogram_bounds", DataType::AnyArray, true),
        ColumnSchema::new("correlation", DataType::Real, true),
        ColumnSchema::new("most_common_elems", DataType::AnyArray, true),
        ColumnSchema::new("most_common_elem_freqs", DataType::RealArray, true),
        ColumnSchema::new("elem_count_histogram", DataType::RealArray, true),
        ColumnSchema::new("range_length_histogram", DataType::AnyArray, true),
        ColumnSchema::new("range_empty_frac", DataType::Real, true),
        ColumnSchema::new("range_bounds_histogram", DataType::AnyArray, true),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for name in cat.visible_relation_keys() {
        // 9.0.0 (C9) — the loop walks KEYS; `name` is the name a client
        // reads and `nsp` the schema column it belongs in, while every
        // catalog lookup below goes on `key`.
        let key = name;
        let nsp = alloc::string::String::from(cat.listed_schema(&key));
        let Some(name) = cat.listed_name(&key).map(alloc::string::String::from) else {
            continue;
        };
        if crate::is_internal_table_name(&name) {
            continue;
        }
        let Some(t) = cat.get(&key) else {
            continue;
        };
        let row_count = t.rows().len();
        #[allow(clippy::cast_precision_loss)]
        for col in &t.schema().columns {
            let Some(cs) = stats.get(&key, &col.name) else {
                continue;
            };
            let distinct = if row_count > 0 && cs.n_distinct as usize == row_count {
                -1.0
            } else {
                cs.n_distinct as f64
            };
            // The histogram SPG really has, rendered as PG renders it.
            let bounds = if cs.histogram_bounds.is_empty() {
                Value::Null
            } else {
                Value::text(alloc::format!("{{{}}}", cs.histogram_bounds.join(",")))
            };
            rows.push(Row::new(alloc::vec![
                Value::text(nsp.clone()),
                Value::text(name.clone()),
                Value::text(col.name.clone()),
                Value::Bool(false),
                Value::Real(cs.null_frac), // null_frac
                Value::Int(0),
                Value::Real(distinct as f32), // n_distinct
                Value::Null,
                Value::Null,
                bounds,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ]));
        }
    }
    (schema, rows)
}

pub(crate) fn synth_pg_statistic(
    cat: &Catalog,
    stats: &crate::statistics::Statistics,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("starelid", DataType::Oid, false),
        ColumnSchema::new("staattnum", DataType::SmallInt, false),
        ColumnSchema::new("stainherit", DataType::Bool, false),
        ColumnSchema::new("stanullfrac", DataType::Real, false),
        ColumnSchema::new("stawidth", DataType::Int, false),
        ColumnSchema::new("stadistinct", DataType::Real, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    let mut starelid: i64 = 16384;
    for name in cat.visible_relation_keys() {
        // 9.0.0 (C9) — the loop walks KEYS; `name` is the name a client
        // reads and `nsp` the schema column it belongs in, while every
        // catalog lookup below goes on `key`.
        let key = name;
        let nsp = alloc::string::String::from(cat.listed_schema(&key));
        let Some(name) = cat.listed_name(&key).map(alloc::string::String::from) else {
            continue;
        };
        if crate::is_internal_table_name(&name) {
            continue;
        }
        let Some(t) = cat.get(&key) else {
            continue;
        };
        // v7.38.18 — the REAL statistics, and only for columns ANALYZE
        // has actually visited.
        //
        // This emitted one all-zero row per column of every table,
        // analysed or not. A test could therefore assert a row count
        // from it and pass without ANALYZE having run — and one did:
        // the S10 pin written an hour earlier said "two columns
        // analysed is two rows of statistics", above a comment reading
        // "'It returned OK' is not evidence that it did anything". It
        // was counting the stub.
        //
        // PostgreSQL has no row here for an un-analysed column, and
        // neither do we now. `stadistinct` follows PG's sign
        // convention: a positive number is a count, a negative one is
        // the ratio to the row count, and -1 means every value differs.
        #[allow(clippy::cast_possible_wrap, clippy::cast_precision_loss)]
        for (i, col) in t.schema().columns.iter().enumerate() {
            let Some(cs) = stats.get(&key, &col.name) else {
                continue;
            };
            let attnum = (i + 1) as i16;
            let row_count = t.rows().len();
            let distinct = if row_count > 0 && cs.n_distinct as usize == row_count {
                -1.0
            } else {
                cs.n_distinct as f64
            };
            rows.push(Row::new(alloc::vec![
                Value::BigInt(starelid),
                Value::SmallInt(attnum),
                Value::Bool(false),
                Value::Real(cs.null_frac), // stanullfrac
                Value::Int(0),
                Value::Real(distinct as f32), // stadistinct
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
/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows. The rows and the
/// columns come from one place, so `pg_attribute` cannot describe
/// a relation differently from how it answers.
pub(crate) fn pg_stat_io_schema() -> Vec<ColumnSchema> {
    alloc::vec![
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
    ]
}

pub(crate) fn synth_pg_stat_io(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_stat_io_schema();
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
/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows. The rows and the
/// columns come from one place, so `pg_attribute` cannot describe
/// a relation differently from how it answers.
pub(crate) fn pg_stat_user_functions_schema() -> Vec<ColumnSchema> {
    alloc::vec![
        ColumnSchema::new("funcid", DataType::BigInt, false),
        ColumnSchema::new("schemaname", DataType::Name, false),
        ColumnSchema::new("funcname", DataType::Text, false),
        ColumnSchema::new("calls", DataType::BigInt, false),
        ColumnSchema::new("total_time", DataType::Float, false),
        ColumnSchema::new("self_time", DataType::Float, false),
    ]
}

pub(crate) fn synth_pg_stat_user_functions(
    _cat: &Catalog,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_stat_user_functions_schema();
    let rows: Vec<Row<'static>> = Vec::new();
    (schema, rows)
}

/// v7.39 (read01 round 53) — the pg_am oid backing an index, derived from what
/// the index ACTUALLY is. `USING hash` / `USING gist` are accepted but built as
/// btree, so they report btree — the catalog never claims an AM SPG lacks.
/// 9.0.0 — the access-method oid an index reports: the one it ASKED for
/// when SPG recorded a declaration, else the one it is. See
/// `Index::declared_am`; PostgreSQL's oids, measured on 18.6.
pub(crate) fn am_oid_of_index(idx: &spg_storage::Index) -> i64 {
    match idx.declared_am.as_deref() {
        Some("gist") => 783,
        Some("spgist") => 4000,
        Some("hash") => 405,
        Some("gin") => 2742,
        Some("brin") => 3580,
        _ => am_oid_of(&idx.kind),
    }
}

pub(crate) const fn am_oid_of(kind: &spg_storage::IndexKind) -> i64 {
    use spg_storage::IndexKind as K;
    match kind {
        K::Nsw(_) => 0, // hnsw is an extension AM; no core oid
        K::Brin { .. } => 3580,
        K::Gin(_) | K::GinTrgm(_) | K::GinFulltext(_) | K::GinJsonb(_) => 2742,
        _ => 403, // btree
    }
}

/// 9.0.0 — the name of the access method an oid names, for the
/// definitions SPG renders from a `CatalogIndex` alone.
pub(crate) const fn am_name_of_oid(oid: i64) -> &'static str {
    match oid {
        3580 => "brin",
        2742 => "gin",
        // 9.0.0 — the three SPG backs with a B-tree but reports as asked.
        783 => "gist",
        4000 => "spgist",
        405 => "hash",
        _ => "btree",
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
/// 9.0.0 — an access method's `amhandler`, as a `regproc`. Every one
/// read `-` (oid 0), where PostgreSQL 18.6 names `bthandler`,
/// `ginhandler` and the rest — the same gap `pg_type`'s I/O columns
/// had, and the functions are in `pg_proc` now.
fn am_handler(name: &str) -> Value<'static> {
    let oid = PG_PROC_FUNCS
        .iter()
        .find(|(_, n, ..)| *n == name)
        .map_or(0, |(o, ..)| *o);
    Value::RegProc(oid, alloc::string::String::from(name).into_boxed_str())
}

pub(crate) fn synth_pg_am(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("amname", DataType::Name, false),
        ColumnSchema::new("amhandler", DataType::RegProc, false),
        ColumnSchema::new("amtype", DataType::Char1, false), // 't' table / 'i' index
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
            am_handler("heap_tableam_handler"), // amhandler
            char1("t"),                         // amtype
        ]),
        Row::new(alloc::vec![
            Value::BigInt(403),
            Value::text("btree"),
            am_handler("bthandler"), // amhandler
            char1("i"),              // amtype
        ]),
        Row::new(alloc::vec![
            Value::BigInt(405),
            Value::text("hash"),
            am_handler("hashhandler"), // amhandler
            char1("i"),                // amtype
        ]),
        Row::new(alloc::vec![
            Value::BigInt(783),
            Value::text("gist"),
            am_handler("gisthandler"), // amhandler
            char1("i"),                // amtype
        ]),
        Row::new(alloc::vec![
            Value::BigInt(2742),
            Value::text("gin"),
            am_handler("ginhandler"), // amhandler
            char1("i"),               // amtype
        ]),
        Row::new(alloc::vec![
            Value::BigInt(4000),
            Value::text("spgist"),
            am_handler("spghandler"), // amhandler
            char1("i"),               // amtype
        ]),
        Row::new(alloc::vec![
            Value::BigInt(3580),
            Value::text("brin"),
            am_handler("brinhandler"), // amhandler
            char1("i"),                // amtype
        ]),
    ];
    (schema, rows)
}

/// v7.37.24 (24.14) — synthesise `pg_catalog.pg_collation`.
/// PG's collation list. ORMs that bind TEXT columns to a
/// language-specific collation read this at handshake to map
/// names → OIDs. SPG ships the three PG-standard collations
/// (`default` / `C` / `POSIX`); the view shape lets monitoring
/// queries and pg_dump's COLLATE-restoration query resolve.
///
/// v7.38.18 — the two sentences that used to follow are gone
/// because both had become false. They read "every TEXT column
/// uses `default` so column-level COLLATE clauses parse but
/// don't alter sort order" and "v7.37.x doesn't yet support
/// per-locale ICU collations". Measured on this build: a column
/// declared `COLLATE "en_US.utf8"` orders `apple, client,
/// DateStyle, Zebra` — PG 18.4's answer to the character — and
/// `<`, `min()` and `information_schema.columns` all agree with
/// it. `collate.rs` carries a full ICU collator calibrated
/// against PG over all 880 of its collation names.
///
/// What remains true is that this TABLE still lists three rows
/// where PG lists 880, so a client that looks a collation up by
/// name is told it does not exist while the engine performs it.
/// That is the same disagreement `pg_settings` had, recorded
/// rather than fixed here: PG's set is its host's locales, and
/// listing that container's 880 would claim SPG has exactly
/// those. Naming what this build can perform needs a source for
/// the candidate names, which is its own piece of work.
pub(crate) fn synth_pg_collation(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("collname", DataType::Name, false),
        ColumnSchema::new("collnamespace", DataType::Oid, false),
        ColumnSchema::new("collowner", DataType::Oid, false),
        ColumnSchema::new("collprovider", DataType::Char1, false), // 'b'/'c'/'i'
        ColumnSchema::new("collisdeterministic", DataType::Bool, false),
        ColumnSchema::new("collencoding", DataType::Int, false),
        ColumnSchema::new("collcollate", DataType::Text, true),
        ColumnSchema::new("collctype", DataType::Text, true),
        // v7.39 (round 543) — PG18 tail columns, measured.
        // PG18 reads all three NULL for C / POSIX / default —
        // colllocale carries a name only for an ICU collation.
        ColumnSchema::new("colllocale", DataType::Text, true),
        ColumnSchema::new("collicurules", DataType::Text, true),
        ColumnSchema::new("collversion", DataType::Text, true),
    ];
    // v7.38.18 (G1) — every collation PG 18.4 publishes that this build
    // can actually perform.
    //
    // Three rows stood here, and a column declared `COLLATE
    // "en_US.utf8"` therefore worked while the catalogue said the
    // collation did not exist — the same disagreement `pg_settings` had.
    //
    // The candidate list is PG's; the filter is ours. A name this build
    // cannot perform is NOT emitted, because a row here is a promise
    // that `COLLATE <name>` will be honoured, and `collate::is_supported`
    // is the only thing that can keep it.
    let rows: Vec<Row<'static>> = crate::collation_catalog::PG_COLLATIONS
        .iter()
        .filter(|(_, name, provider, ..)| {
            // `default` names whatever the database was created with and
            // is always performable; the rest have to answer for
            // themselves.
            *provider == "d" || crate::collate::is_supported(name)
        })
        .map(
            |&(oid, name, provider, deterministic, encoding, cc, ct, loc, icurules, version)| {
                let text =
                    |v: Option<&str>| v.map_or(Value::Null, |s| Value::text::<String>(s.into()));
                Row::new(alloc::vec![
                    Value::BigInt(i64::from(oid)),
                    Value::text::<String>(name.into()),
                    Value::BigInt(11), // collnamespace — pg_catalog
                    Value::BigInt(10), // collowner
                    char1(provider),   // collprovider
                    Value::Bool(deterministic),
                    Value::Int(encoding),
                    text(cc),
                    text(ct),
                    text(loc),
                    text(icurules),
                    text(version),
                ])
            },
        )
        .collect();
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
/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows. The rows and the
/// columns come from one place, so `pg_attribute` cannot describe
/// a relation differently from how it answers.
pub(crate) fn pg_stat_archiver_schema() -> Vec<ColumnSchema> {
    alloc::vec![
        ColumnSchema::new("archived_count", DataType::BigInt, false),
        ColumnSchema::new("last_archived_wal", DataType::Text, true),
        ColumnSchema::new("last_archived_time", DataType::Timestamptz, true),
        ColumnSchema::new("failed_count", DataType::BigInt, false),
        ColumnSchema::new("last_failed_wal", DataType::Text, true),
        ColumnSchema::new("last_failed_time", DataType::Timestamptz, true),
        ColumnSchema::new("stats_reset", DataType::Timestamptz, true),
    ]
}

pub(crate) fn synth_pg_stat_archiver(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_stat_archiver_schema();
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
/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows. The rows and the
/// columns come from one place, so `pg_attribute` cannot describe
/// a relation differently from how it answers.
pub(crate) fn pg_stat_replication_schema() -> Vec<ColumnSchema> {
    alloc::vec![
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
    ]
}

pub(crate) fn synth_pg_stat_replication(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_stat_replication_schema();
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
/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows. The rows and the
/// columns come from one place, so `pg_attribute` cannot describe
/// a relation differently from how it answers.
pub(crate) fn pg_stat_slru_schema() -> Vec<ColumnSchema> {
    alloc::vec![
        ColumnSchema::new("name", DataType::Text, false),
        ColumnSchema::new("blks_zeroed", DataType::BigInt, false),
        ColumnSchema::new("blks_hit", DataType::BigInt, false),
        ColumnSchema::new("blks_read", DataType::BigInt, false),
        ColumnSchema::new("blks_written", DataType::BigInt, false),
        ColumnSchema::new("blks_exists", DataType::BigInt, false),
        ColumnSchema::new("flushes", DataType::BigInt, false),
        ColumnSchema::new("truncates", DataType::BigInt, false),
        ColumnSchema::new("stats_reset", DataType::Timestamptz, true),
    ]
}

pub(crate) fn synth_pg_stat_slru(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_stat_slru_schema();
    (schema, Vec::new())
}

/// v7.38 (read01 P3.15) — `pg_catalog.pg_stat_subscription_stats`. One
/// row per subscription would carry apply/sync error + conflict counts;
/// SPG doesn't track them yet, so this is an empty shape-stable shell.
/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows. The rows and the
/// columns come from one place, so `pg_attribute` cannot describe
/// a relation differently from how it answers.
pub(crate) fn pg_stat_subscription_stats_schema() -> Vec<ColumnSchema> {
    alloc::vec![
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
    ]
}

pub(crate) fn synth_pg_stat_subscription_stats(
    _cat: &Catalog,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_stat_subscription_stats_schema();
    (schema, Vec::new())
}

/// v7.38 (read01 P3.14) — `pg_catalog.pg_stat_checkpointer` (PG 17+).
/// SPG checkpoints WAL/segments on its own schedule; the cumulative
/// counters aren't wired yet, so this is a shape-stable single row of
/// zeros so monitoring queries parse.
/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows. The rows and the
/// columns come from one place, so `pg_attribute` cannot describe
/// a relation differently from how it answers.
pub(crate) fn pg_stat_checkpointer_schema() -> Vec<ColumnSchema> {
    alloc::vec![
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
    ]
}

pub(crate) fn synth_pg_stat_checkpointer(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_stat_checkpointer_schema();
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
/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows. The rows and the
/// columns come from one place, so `pg_attribute` cannot describe
/// a relation differently from how it answers.
pub(crate) fn pg_stat_wal_schema() -> Vec<ColumnSchema> {
    alloc::vec![
        ColumnSchema::new("wal_records", DataType::BigInt, false),
        ColumnSchema::new("wal_fpi", DataType::BigInt, false),
        ColumnSchema::new("wal_bytes", DataType::BigInt, false),
        ColumnSchema::new("wal_buffers_full", DataType::BigInt, false),
        ColumnSchema::new("stats_reset", DataType::Timestamptz, true),
    ]
}

pub(crate) fn synth_pg_stat_wal(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_stat_wal_schema();
    let rows = alloc::vec![Row::new(alloc::vec![
        Value::BigInt(0),
        Value::BigInt(0),
        Value::BigInt(0),
        Value::BigInt(0),
        Value::Null,
    ])];
    (schema, rows)
}

/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows. The rows and the
/// columns come from one place, so `pg_attribute` cannot describe
/// a relation differently from how it answers.
pub(crate) fn pg_stat_bgwriter_schema() -> Vec<ColumnSchema> {
    alloc::vec![
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
    ]
}

pub(crate) fn synth_pg_stat_bgwriter(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_stat_bgwriter_schema();
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
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("spcname", DataType::Name, false),
        ColumnSchema::new("spcowner", DataType::Oid, false),
        ColumnSchema::new("spcacl", DataType::AclItemArray, true),
        ColumnSchema::new("spcoptions", DataType::TextArray, true),
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
/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows. The rows and the
/// columns come from one place, so `pg_attribute` cannot describe
/// a relation differently from how it answers.
pub(crate) fn pg_stat_user_indexes_schema() -> Vec<ColumnSchema> {
    alloc::vec![
        ColumnSchema::new("relid", DataType::BigInt, false),
        ColumnSchema::new("indexrelid", DataType::Oid, false),
        ColumnSchema::new("schemaname", DataType::Name, false),
        ColumnSchema::new("relname", DataType::Name, false),
        ColumnSchema::new("indexrelname", DataType::Text, false),
        ColumnSchema::new("idx_scan", DataType::BigInt, false),
        ColumnSchema::new("idx_tup_read", DataType::BigInt, false),
        ColumnSchema::new("idx_tup_fetch", DataType::BigInt, false),
    ]
}

pub(crate) fn synth_pg_stat_user_indexes(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_stat_user_indexes_schema();
    let mut rows: Vec<Row<'static>> = Vec::new();
    // v7.39.13 — through the one enumeration, so `indexrelid` still
    // means the same object `pg_class.oid` and `pg_index.indexrelid` do.
    let mut relid: i64 = 16384;
    let mut relid_of: alloc::collections::BTreeMap<alloc::string::String, i64> =
        alloc::collections::BTreeMap::new();
    for tname in cat.visible_relation_keys() {
        // 9.0.0 (C9) — the loop walks KEYS; `tname` is the name a client
        // reads and `nsp` the schema column it belongs in, while every
        // catalog lookup below goes on `key`.
        let key = tname;
        let nsp = alloc::string::String::from(cat.listed_schema(&key));
        let Some(tname) = cat.listed_name(&key).map(alloc::string::String::from) else {
            continue;
        };
        if crate::is_internal_table_name(&tname) {
            continue;
        }
        relid_of.insert(tname.clone(), relid);
        relid = relid.saturating_add(1);
    }
    for ci in catalog_indexes(cat) {
        let Some(relid) = relid_of.get(&ci.table).copied() else {
            continue;
        };
        {
            let indexrelid = ci.oid;
            let tname = &ci.table;
            rows.push(Row::new(alloc::vec![
                Value::BigInt(relid),
                Value::BigInt(indexrelid),
                Value::text("public"),
                Value::Text(alloc::borrow::Cow::Owned(tname.clone())),
                Value::Text(alloc::borrow::Cow::Owned(ci.name.clone())),
                Value::BigInt(0), // idx_scan
                Value::BigInt(0), // idx_tup_read
                Value::BigInt(0), // idx_tup_fetch
            ]));
        }
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
/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows.
pub(crate) fn pg_stat_user_tables_schema() -> Vec<ColumnSchema> {
    alloc::vec![
        ColumnSchema::new("relid", DataType::BigInt, false),
        ColumnSchema::new("schemaname", DataType::Name, false),
        ColumnSchema::new("relname", DataType::Name, false),
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
    ]
}

pub(crate) fn synth_pg_stat_user_tables(
    cat: &Catalog,
    write_stats: &alloc::collections::BTreeMap<alloc::string::String, (u64, u64, u64)>,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_stat_user_tables_schema();
    let mut rows: Vec<Row<'static>> = Vec::new();
    let mut relid: i64 = 16384; // PG user-relation OID floor
    for name in cat.visible_relation_keys() {
        // 9.0.0 (C9) — the loop walks KEYS; `name` is the name a client
        // reads and `nsp` the schema column it belongs in, while every
        // catalog lookup below goes on `key`.
        let key = name;
        let nsp = alloc::string::String::from(cat.listed_schema(&key));
        let Some(name) = cat.listed_name(&key).map(alloc::string::String::from) else {
            continue;
        };
        if crate::is_internal_table_name(&name) {
            continue;
        }
        let Some(t) = cat.get(&key) else {
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
        let (ins, upd, del) = write_stats.get(&key).copied().unwrap_or((0, 0, 0));
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
            Value::text(nsp.clone()),
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
///   * temp_files / temp_bytes (BigInt) — disk-spill counters, one
///     file per sort run and the bytes they hold (round 884)
///   * blk_read_time / blk_write_time (Float) — accumulated
///     I/O wait time (0 until per-statement timing lands)
/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows. The rows and the
/// columns come from one place, so `pg_attribute` cannot describe
/// a relation differently from how it answers.
pub(crate) fn pg_stat_database_schema() -> Vec<ColumnSchema> {
    alloc::vec![
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
    ]
}

pub(crate) fn synth_pg_stat_database(
    eng: &Engine,
    tup_inserted: u64,
    tup_updated: u64,
    tup_deleted: u64,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_stat_database_schema();
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
        for name in cat.visible_relation_keys() {
            // 9.0.0 (C9) — the loop walks KEYS; `name` is the name a client
            // reads and `nsp` the schema column it belongs in, while every
            // catalog lookup below goes on `key`.
            let key = name;
            let nsp = alloc::string::String::from(cat.listed_schema(&key));
            let Some(name) = cat.listed_name(&key).map(alloc::string::String::from) else {
                continue;
            };
            if let Some(t) = cat.get(&key) {
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
    // v7.37 (round 884) — the name `current_database()` answers, read from
    // the same place it reads. `pg_database` was given this treatment in
    // round 474 for exactly this reason ("so the two cannot drift apart
    // again"); its sibling here was missed, so
    // `SELECT ... FROM pg_stat_database WHERE datname = current_database()`
    // — the shape every monitoring dashboard writes — matched nothing at
    // all and returned an empty result rather than an error.
    let datname = eng
        .session_params
        .get("spg.database")
        .cloned()
        .unwrap_or_else(|| alloc::string::String::from("spg"));
    // v7.37 (round 884) — real spill counters. PG's `temp_files` counts one
    // file per sort run and `temp_bytes` what they hold; a monitoring query
    // watches the pair to find the queries that outgrow `work_mem`. These
    // read 0 while the sorter was writing 26 runs a query.
    let (temp_files, temp_bytes) = {
        use core::sync::atomic::Ordering;
        (
            eng.spill_stats.files.load(Ordering::Relaxed),
            eng.spill_stats.bytes.load(Ordering::Relaxed),
        )
    };
    let rows = alloc::vec![Row::new(alloc::vec![
        Value::BigInt(16384),
        Value::text(datname),
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
        Value::BigInt(0), // conflicts (PG: replication-conflict count)
        Value::BigInt(i64::try_from(temp_files).unwrap_or(i64::MAX)), // temp_files
        Value::BigInt(i64::try_from(temp_bytes).unwrap_or(i64::MAX)), // temp_bytes
        Value::BigInt(0), // deadlocks (SPG single-writer; always 0)
        Value::BigInt(0), // checksum_failures
        Value::Null,      // checksum_last_failure
        Value::Float(0.0), // blk_read_time
        Value::Float(0.0), // blk_write_time
        Value::Float(0.0), // session_time
        Value::Float(0.0), // active_time
        Value::Float(0.0), // idle_in_transaction_time
        Value::BigInt(0), // sessions
        Value::BigInt(0), // sessions_abandoned
        Value::BigInt(0), // sessions_fatal
        Value::BigInt(0), // sessions_killed
        Value::BigInt(0), // parallel_workers_to_launch
        Value::BigInt(0), // parallel_workers_launched
        Value::Null,      // stats_reset (never reset)
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
/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows. The rows and the
/// columns come from one place, so `pg_attribute` cannot describe
/// a relation differently from how it answers.
pub(crate) fn pg_subscription_schema() -> Vec<ColumnSchema> {
    alloc::vec![
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("subdbid", DataType::BigInt, false),
        ColumnSchema::new("subname", DataType::Text, false),
        ColumnSchema::new("subowner", DataType::BigInt, false),
        ColumnSchema::new("subenabled", DataType::Bool, false),
        ColumnSchema::new("subconninfo", DataType::Text, false),
        ColumnSchema::new("subslotname", DataType::Text, true),
        ColumnSchema::new("subpublications", DataType::Text, false),
        ColumnSchema::new("subbinary", DataType::Bool, false),
        ColumnSchema::new("substream", DataType::Bool, false),
        // 8.0.2 — the eight PostgreSQL 18.6 also has, because pg_dump
        // asks for seven of them BY NAME.
        //
        // It asks only when it believes the connected role is a
        // superuser, and it learns that from the `is_superuser`
        // ParameterStatus — one of the ten this release added to the
        // startup packet for the timezone crash. So a dump that had
        // never issued this query began issuing it, and
        // `column s.subsynccommit does not exist` took `pg_dump` to
        // exit 1 against a server it had dumped a release earlier.
        //
        // Nothing here is a subscription feature: this catalog is a
        // stub with a row per `CREATE SUBSCRIPTION` and the defaults
        // below are PostgreSQL's own for a freshly created one. What
        // was missing was the SHAPE, and a tool reading a catalog reads
        // the shape whether or not there are rows in it.
        ColumnSchema::new("subskiplsn", DataType::Text, false),
        ColumnSchema::new("subtwophasestate", DataType::Char1, false),
        ColumnSchema::new("subdisableonerr", DataType::Bool, false),
        ColumnSchema::new("subpasswordrequired", DataType::Bool, false),
        ColumnSchema::new("subrunasowner", DataType::Bool, false),
        ColumnSchema::new("subfailover", DataType::Bool, false),
        ColumnSchema::new("subsynccommit", DataType::Text, false),
        ColumnSchema::new("suborigin", DataType::Text, false),
    ]
}

pub(crate) fn synth_pg_subscription(eng: &Engine) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_subscription_schema();
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
            // PostgreSQL's defaults for a subscription just created.
            Value::text("0/0"), // subskiplsn
            char1("d"),         // subtwophasestate — disabled
            Value::Bool(false), // subdisableonerr
            Value::Bool(true),  // subpasswordrequired
            Value::Bool(false), // subrunasowner
            Value::Bool(false), // subfailover
            Value::text("off"), // subsynccommit
            Value::text("any"), // suborigin
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
/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows.
pub(crate) fn pg_publication_schema() -> Vec<ColumnSchema> {
    alloc::vec![
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("pubname", DataType::Text, false),
        ColumnSchema::new("pubowner", DataType::BigInt, false),
        ColumnSchema::new("puballtables", DataType::Bool, false),
        ColumnSchema::new("pubinsert", DataType::Bool, false),
        ColumnSchema::new("pubupdate", DataType::Bool, false),
        ColumnSchema::new("pubdelete", DataType::Bool, false),
        ColumnSchema::new("pubtruncate", DataType::Bool, false),
        ColumnSchema::new("pubviaroot", DataType::Bool, false),
        // v7.39 (round 543) — PG18's generated-column mode. Measured on
        // a fresh `CREATE PUBLICATION … FOR ALL TABLES`: 'n' (none).
        ColumnSchema::new("pubgencols", DataType::Char1, false),
    ]
}

pub(crate) fn synth_pg_publication(eng: &Engine) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    use spg_sql::ast::PublicationScope;
    let schema = pg_publication_schema();
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
            char1("n"),         // pubgencols
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
/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows. The rows and the
/// columns come from one place, so `pg_attribute` cannot describe
/// a relation differently from how it answers.
pub(crate) fn pg_replication_slots_schema() -> Vec<ColumnSchema> {
    alloc::vec![
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
    ]
}

pub(crate) fn synth_pg_replication_slots(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_replication_slots_schema();
    // v7.39 (round 550) — the slots the catalog actually holds. This
    // was pinned empty, and the create/drop functions answered NULL, so
    // a replication setup script ran clean and made nothing.
    //
    // `wal_status` reads `unreserved` — PG's own word for a slot that
    // no longer holds WAL back, which is the truth here: SPG keeps the
    // record, not the reservation.
    let rows: Vec<Row<'static>> = cat
        .replication_slots()
        .iter()
        .map(|(name, (plugin, slot_type))| {
            Row::new(alloc::vec![
                Value::text(name.clone()),
                if plugin.is_empty() {
                    Value::Null
                } else {
                    Value::text(plugin.clone())
                },
                Value::text(slot_type.clone()),
                Value::BigInt(16384),
                Value::text("spg"),
                Value::Bool(false),
                Value::Bool(false),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::text("unreserved"),
                Value::Null,
            ])
        })
        .collect();
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
                Value::text(information_schema_data_type(*field_type)),
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
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("enumtypid", DataType::Oid, false),
        ColumnSchema::new("enumsortorder", DataType::Real, false),
        ColumnSchema::new("enumlabel", DataType::Name, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    // Synthetic OID bands: enum types start at 50_000; per-label
    // OIDs start at 60_000. Keeps them disjoint from pg_type's
    // user-space scalar OID band (which lives above 16384 but
    // below the 50_000 mark we land enum types into).
    let mut label_oid: i64 = 60_000;
    let (enum_oids, _, _) = user_type_oids(cat);
    for ((_name, def), (_, typid)) in cat.enum_types().iter().zip(enum_oids) {
        for (i, label) in def.labels.iter().enumerate() {
            label_oid = label_oid.saturating_add(1);
            rows.push(Row::new(alloc::vec![
                Value::BigInt(label_oid),
                Value::BigInt(typid),
                Value::Real((i + 1) as f32), // enumsortorder
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
/// v7.40.0 — `mysql` picks which engine's answer this view gives.
///
/// The two disagree, and both are right about themselves. Measured, same
/// table, `PRIMARY KEY (a,b)` and two NOT NULL columns:
///
/// ```text
///   PostgreSQL 18.6            MySQL 9.7.2
///   mc_i_pkey  PRIMARY KEY     PRIMARY  PRIMARY KEY
///   mc_i_a_not_null  CHECK     (absent)
///   mc_i_b_not_null  CHECK     (absent)
/// ```
///
/// PostgreSQL 18 models a NOT NULL column as a real CHECK constraint and
/// lists it; MySQL has no such object. And MySQL calls every primary key
/// `PRIMARY`, whatever the constraint was declared as. Answering one
/// engine's shape on the other's wire is a schema reader's problem, not
/// a cosmetic one — a diff tool reports constraints that are not there.
/// An unnamed foreign key is `<table>_ibfk_<n>` there, the same name
/// `SHOW CREATE TABLE` prints.
pub(crate) fn synth_information_schema_table_constraints(
    cat: &Catalog,
    mysql: bool,
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
    for tname in cat.visible_relation_keys() {
        // 9.0.0 (C9) — the loop walks KEYS; `tname` is the name a client
        // reads and `nsp` the schema column it belongs in, while every
        // catalog lookup below goes on `key`.
        let key = tname;
        let nsp = alloc::string::String::from(cat.listed_schema(&key));
        let Some(tname) = cat.listed_name(&key).map(alloc::string::String::from) else {
            continue;
        };
        let Some(t) = cat.get(&key) else { continue };
        // Uniqueness constraints — both PK and UNIQUE forms.
        for uc in t.schema().uniqueness_constraints.iter() {
            let conname = if mysql && uc.is_primary_key {
                alloc::string::String::from("PRIMARY")
            } else {
                pg_unique_conname(t, uc, &tname)
            };
            let kind = if uc.is_primary_key {
                "PRIMARY KEY"
            } else {
                "UNIQUE"
            };
            rows.push(Row::new(alloc::vec![
                Value::text("spg"),
                Value::text(nsp.clone()),
                Value::text(conname),
                Value::text("spg"),
                Value::text(nsp.clone()),
                Value::text(tname.clone()),
                Value::text(kind),
                Value::text("NO"),
                Value::text("NO"),
                Value::text("YES"),
            ]));
        }
        // v7.39.11 — a bare `CREATE UNIQUE INDEX` is an INDEX, and
        // PostgreSQL lists no constraint for it. This loop used to list
        // one, the same way the `pg_constraint` synthesiser did until
        // this version; sentori found the pair against 7.39.10, where a
        // schema-diff tool comparing the two sides saw a constraint on
        // ours that does not exist on PostgreSQL's.
        //
        // An index that BACKS a constraint still gets its row from the
        // uniqueness-constraint loop above, which is where the
        // constraint lives.
        // Foreign keys.
        for (fk_i, fk) in t.schema().foreign_keys.iter().enumerate() {
            let conname = fk.name.clone().unwrap_or_else(|| {
                if mysql {
                    alloc::format!("{tname}_ibfk_{}", fk_i + 1)
                } else {
                    pg_fk_conname(t, fk, &tname)
                }
            });
            rows.push(Row::new(alloc::vec![
                Value::text("spg"),
                Value::text(nsp.clone()),
                Value::text(conname),
                Value::text("spg"),
                Value::text(nsp.clone()),
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
                Value::text(nsp.clone()),
                Value::text(check_names[ci].clone()),
                Value::text("spg"),
                Value::text(nsp.clone()),
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
            // MySQL has no NOT NULL constraint object to list.
            if col.nullable || mysql {
                continue;
            }
            rows.push(Row::new(alloc::vec![
                Value::text("spg"),
                Value::text(nsp.clone()),
                Value::text(alloc::format!("{tname}_{}_not_null", col.name)),
                Value::text("spg"),
                Value::text(nsp.clone()),
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

/// v7.39.13 — every object that gets an index OID, enumerated ONCE.
///
/// Five places walked `t.indices()` from `OID_INDEX_BASE` and had to
/// agree on the order, and one of them says so in a comment: "Index
/// oids follow the SAME sequence synth_pg_index_raw uses". They agreed
/// on what they enumerated and were all wrong about the same thing.
///
/// SPG builds a SINGLE-column index for a composite constraint added
/// by `ALTER TABLE` — the form `pg_dump` emits — while the constraint
/// records every column. `pg_index` therefore guessed which constraint
/// an index backed by PREFIX-matching its columns, and reported the
/// guess. Measured against PG 18.6, reported by sentori:
///
/// ```text
///                                 PG 18.6                 SPG 7.39.12
///   PK(a,b), then INDEX ix(a)   ix f/f/1, pkey t/t/2   BOTH t/t/1
///   INDEX ix(a), then PK(a,b)   ix f/f/1, pkey t/t/2   ix only
///   is ix actually unique?      no                     says yes
/// ```
///
/// The third line is the dangerous one: an index that is not unique,
/// reporting that it is. A migration guard that asserts uniqueness
/// before relying on it gets a yes and is wrong — the opposite
/// direction from the defect 7.39.11 fixed, which was a real key
/// reporting NOT unique and merely raised a false alarm.
///
/// So nothing prefix-matches any more. A storage index reports its own
/// columns and its own uniqueness, and each uniqueness constraint gets
/// the row PostgreSQL gives it: its own columns, unique, primary when
/// it says so. A storage index whose columns EXACTLY equal a
/// constraint's is that constraint's index and carries its flags,
/// which is how an inline `PRIMARY KEY (a, b)` stays one row.
pub(crate) struct CatalogIndex {
    pub oid: i64,
    pub table: alloc::string::String,
    pub name: alloc::string::String,
    /// 0-based column positions on the parent table.
    pub columns: Vec<usize>,
    pub included: Vec<usize>,
    pub is_unique: bool,
    pub is_primary: bool,
    /// The access method's OID, already resolved — `IndexKind` carries
    /// the index's data, and a synthesised row has none to carry.
    pub am_oid: i64,
    pub partial_predicate: Option<alloc::string::String>,
    pub expression: Option<alloc::string::String>,
    pub nulls_not_distinct: bool,
    /// `false` for the row a constraint gets when no storage index has
    /// exactly its columns — it exists in PostgreSQL and not in SPG's
    /// storage, and the difference matters to anything that asks the
    /// index for statistics.
    pub is_storage: bool,
    /// 8.0.3 — the position, in its table's uniqueness constraints, of the
    /// constraint this index backs. `pg_constraint.conindid` reads it; it
    /// was 0, and `pg_dump` joins on that column to tell a constraint's
    /// index from a bare one — so every primary key and UNIQUE constraint
    /// dumped as `CREATE UNIQUE INDEX`, and restored as one.
    pub backs_constraint: Option<usize>,
    /// 9.0.0 — `ON ONLY`: this index is a partitioned parent's
    /// DECLARATION, which PostgreSQL carries as a relation of its own
    /// and `pg_dump` writes as `CREATE INDEX … ON ONLY …`. SPG kept it
    /// as a template string and listed nothing, so a dump emitted only
    /// the partitions' indexes and a partition created after the
    /// restore inherited none.
    pub only: bool,
    /// 9.0.0 — each key part's expression text, aligned with `columns`;
    /// `None` where the part is the column itself. `indkey` is 0 for an
    /// expression part and `indexprs` lists them all.
    pub part_expressions: Vec<Option<alloc::string::String>>,
    /// 9.0.0 — each key part's `DESC` / `NULLS FIRST`, aligned with
    /// `columns`, for `indoption`.
    pub part_orders: Vec<spg_storage::KeyOrder>,
    /// 9.0.0 — the operator class each key part was declared with,
    /// aligned with `columns`; `None` where the index named none and
    /// PostgreSQL picks the type's default. `indclass` reads it.
    pub part_opclasses: Vec<Option<alloc::string::String>>,
}

/// PostgreSQL's own name for a constraint's index.
fn constraint_index_name(
    table: &str,
    uc: &spg_storage::UniquenessConstraint,
    cols: &[String],
) -> alloc::string::String {
    // PostgreSQL names a constraint's index after the constraint when
    // one was named, and generates the rest.
    if let Some(n) = &uc.name {
        return n.clone();
    }
    if uc.is_primary_key {
        return alloc::format!("{table}_pkey");
    }
    let mut n = alloc::string::String::from(table);
    for c in cols {
        n.push('_');
        n.push_str(c);
    }
    n.push_str("_key");
    n
}

pub(crate) fn catalog_indexes(cat: &spg_storage::Catalog) -> Vec<CatalogIndex> {
    let mut out: Vec<CatalogIndex> = Vec::new();
    let mut oid = OID_INDEX_BASE;
    for tname in cat.visible_relation_keys() {
        // 9.0.0 (C9) — the loop walks KEYS; `tname` is the name a client
        // reads and `nsp` the schema column it belongs in, while every
        // catalog lookup below goes on `key`.
        let key = tname;
        let nsp = alloc::string::String::from(cat.listed_schema(&key));
        let Some(tname) = cat.listed_name(&key).map(alloc::string::String::from) else {
            continue;
        };
        let Some(t) = cat.get(&key) else { continue };
        let ucs = &t.schema().uniqueness_constraints;
        let col_names: Vec<alloc::string::String> =
            t.schema().columns.iter().map(|c| c.name.clone()).collect();
        let mut claimed: Vec<bool> = alloc::vec![false; ucs.len()];
        for idx in t.indices() {
            // v7.39.13 — SPG's own probe index for a constraint's
            // non-leading columns is not an index anyone declared, and
            // PostgreSQL has no equivalent to show. Skipped because
            // storage RECORDS that SPG built it — the fact the two
            // versions before this one tried to infer from a name and
            // then from a column prefix.
            if idx.constraint_internal {
                continue;
            }
            oid += 1;
            let columns: Vec<usize> = core::iter::once(idx.column_position)
                .chain(idx.extra_column_positions.iter().copied())
                .collect();
            // v7.39.13 — the index says whether it is a constraint's,
            // and only then are the columns matched to find WHICH.
            //
            // Matching on columns alone — by prefix in 7.39.12, by
            // equality in the first cut of this — renamed
            // `CREATE INDEX idx_d_a ON d (a)` to the name of the
            // `UNIQUE (a)` beside it, and reported an expression index
            // on `(a + 1)` as the primary key, because its stored
            // column position is the key's.
            let exact = if idx.constraint_backing {
                ucs.iter().position(|uc| uc.columns == columns)
            } else {
                None
            };
            if let Some(i) = exact {
                claimed[i] = true;
            }
            let uc = exact.map(|i| &ucs[i]);
            // A constraint's index carries PostgreSQL's name for it.
            // SPG generates `t_a_pkey_0_0` for an inline composite key;
            // PostgreSQL calls that index `t_pkey`, and a schema reader
            // matching names against `pg_constraint` finds nothing under
            // the other spelling.
            let name = uc.map_or_else(
                || idx.name.clone(),
                |u| {
                    let cols: Vec<alloc::string::String> = u
                        .columns
                        .iter()
                        .filter_map(|p| col_names.get(*p).cloned())
                        .collect();
                    constraint_index_name(&tname, u, &cols)
                },
            );
            out.push(CatalogIndex {
                oid,
                table: tname.clone(),
                name,
                columns,
                included: idx.included_columns.clone(),
                is_unique: idx.is_unique || uc.is_some(),
                is_primary: uc.is_some_and(|u| u.is_primary_key),
                am_oid: am_oid_of_index(idx),
                partial_predicate: idx.partial_predicate.clone(),
                expression: idx.expression.clone(),
                nulls_not_distinct: idx.nulls_not_distinct,
                is_storage: true,
                only: false,
                backs_constraint: exact,
                part_expressions: (0..=idx.extra_column_positions.len())
                    .map(|i| idx.part_expression(i).map(alloc::string::String::from))
                    .collect(),
                part_orders: core::iter::once(spg_storage::KeyOrder {
                    descending: idx.descending,
                    nulls_first: idx.nulls_first,
                })
                .chain(
                    (0..idx.extra_column_positions.len())
                        .map(|i| idx.extra_orders.get(i).copied().unwrap_or_default()),
                )
                .collect(),
                part_opclasses: {
                    let declared = cat.index_opclasses(&idx.name);
                    (0..=idx.extra_column_positions.len())
                        .map(|i| declared.get(i).cloned().flatten())
                        .collect()
                },
            });
        }
        // The constraints no storage index covers exactly. PostgreSQL
        // has an index for every one of them; SPG enforces them without
        // one, and a catalog that omits them tells a schema reader the
        // table has no primary key.
        for (i, uc) in ucs.iter().enumerate() {
            if claimed[i] {
                continue;
            }
            oid += 1;
            let cols: Vec<alloc::string::String> = uc
                .columns
                .iter()
                .filter_map(|p| col_names.get(*p).cloned())
                .collect();
            out.push(CatalogIndex {
                oid,
                table: tname.clone(),
                name: constraint_index_name(&tname, uc, &cols),
                columns: uc.columns.clone(),
                included: Vec::new(),
                is_unique: true,
                is_primary: uc.is_primary_key,
                am_oid: 403, // btree — what PostgreSQL builds for a key
                partial_predicate: None,
                expression: None,
                nulls_not_distinct: uc.nulls_not_distinct,
                is_storage: false,
                only: false,
                backs_constraint: Some(i),
                part_expressions: alloc::vec![None; uc.columns.len()],
                part_orders: alloc::vec![spg_storage::KeyOrder::default(); uc.columns.len()],
                part_opclasses: alloc::vec![None; uc.columns.len()],
            });
        }
        // 9.0.0 — a partitioned parent's index DECLARATION. SPG keeps it
        // as a `CREATE INDEX` template that every partition replays, and
        // listed nothing here: `pg_indexes` had no row for it, so
        // `pg_dump` (which reads `indexdef` from this view) emitted the
        // partitions' indexes alone and a partition created after the
        // restore inherited none. PostgreSQL 18.6 carries the parent's
        // index as a relation of its own and writes it `ON ONLY`.
        if let Some(spg_storage::PartitionRole::Parent {
            index_template_sources,
            ..
        }) = &t.schema().partition_role
        {
            for src in index_template_sources {
                let Ok(spg_sql::ast::Statement::CreateIndex(ci)) =
                    spg_sql::parser::parse_statement(src)
                else {
                    continue;
                };
                let positions: Vec<usize> = core::iter::once(&ci.column)
                    .chain(ci.extra_columns.iter())
                    .filter_map(|c| t.schema().column_position(c))
                    .collect();
                if positions.len() != 1 + ci.extra_columns.len() {
                    continue;
                }
                oid += 1;
                out.push(CatalogIndex {
                    oid,
                    table: tname.clone(),
                    name: ci.name.clone(),
                    part_expressions: alloc::vec![None; positions.len()],
                    part_orders: alloc::vec![
                        spg_storage::KeyOrder::default();
                        positions.len()
                    ],
                    part_opclasses: alloc::vec![None; positions.len()],
                    columns: positions,
                    included: Vec::new(),
                    is_unique: ci.is_unique,
                    is_primary: false,
                    am_oid: match ci.method {
                        spg_sql::ast::IndexMethod::Brin => 3580,
                        _ => 403,
                    },
                    partial_predicate: None,
                    expression: None,
                    nulls_not_distinct: ci.nulls_not_distinct,
                    is_storage: false,
                    backs_constraint: None,
                    only: true,
                });
            }
        }
    }
    out
}

/// v7.39 (round 635) — pg_cast rows need an oid of their own. Above the
/// index range so it cannot collide with a relation's.
// 7.38.1 S5.1 — pg_cast rows are BUILTIN casts, and pg_dump decides
// "user-defined, dump it" by `oid >= 16384` (FirstNormalObjectId).
// The old 200_000 band exported every one of them as a CREATE CAST
// statement (with bogus-value warnings for the method fields) that a
// real PG would refuse to restore. 10_000 sits inside PG's reserved
// band and clear of every real builtin-cast oid.
pub(crate) const OID_CAST_BASE: i64 = 10_000;
pub(crate) const OID_SEQ_BASE: i64 = 300_000;
/// v7.39 (round 342, V65) — user functions, keyed by signature the way
/// `pg_proc` iterates them.
pub(crate) const OID_FUNC_BASE: i64 = 400_000;
/// v7.39 (round 542) — pg_trigger row oids.
pub(crate) const OID_TRIGGER_BASE: i64 = 600_000;
/// 8.0.3 — domain constraints' own band (see `synth_pg_constraint`).
pub(crate) const OID_DOMAIN_CONSTRAINT_BASE: i64 = 800_000;

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
    for ci in catalog_indexes(cat) {
        if ci.oid == oid {
            return Some(ci.name);
        }
    }
    let mut seq_oid = OID_SEQ_BASE;
    for (name, _) in crate::sequence::catalog_sequences(cat) {
        let Some(name) = cat.listed_name(&name) else {
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
    for ci in catalog_indexes(cat) {
        if ci.name == bare {
            return Some(ci.oid);
        }
    }
    let mut seq_oid = OID_SEQ_BASE;
    for (name, _) in crate::sequence::catalog_sequences(cat) {
        let Some(name) = cat.listed_name(&name) else {
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
pub(crate) fn schema_name_for_oid(cat: &Catalog, oid: i64) -> Option<alloc::string::String> {
    // 9.0.0 (C9) — a schema `CREATE SCHEMA` made.
    if let Some(i) = oid.checked_sub(USER_NS_OID_BASE)
        && (0..USER_NS_OID_SPAN).contains(&i)
        && let Ok(i) = usize::try_from(i)
        && let Some(name) = cat.user_schemas().iter().nth(i)
    {
        return Some(name.clone());
    }
    // v7.38.14 — a session's temporary namespace, which PG publishes as
    // `pg_temp_N` and SPG reported as `public`.
    //
    // The objects themselves have been session-scoped since round 469 --
    // they carry a per-session name prefix, and another connection cannot
    // see or use them -- so the LIFETIME was never the defect the ledger
    // recorded. What was wrong is what the catalog SAYS: a schema-diff or
    // migration tool reading `pg_class`/`pg_namespace` saw a temporary
    // object sitting in `public` and had no way to tell it apart from a
    // permanent one.
    if let Some(sid) = oid.checked_sub(TEMP_NS_OID_BASE)
        && (0..TEMP_NS_OID_SPAN).contains(&sid)
    {
        return Some(alloc::format!("pg_temp_{sid}"));
    }
    let name = match oid {
        11 => "pg_catalog",
        2200 => "public",
        13000 => "information_schema",
        _ => return None,
    };
    Some(alloc::string::String::from(name))
}

/// v7.38.14 — the oid a session's `pg_temp_N` namespace takes. Chosen
/// above every oid this catalog hands out so the two spaces cannot
/// collide, and derived from the session id so the name is stable for as
/// long as the session is.
pub(crate) const TEMP_NS_OID_BASE: i64 = 900_000;
/// How many sessions the temp-namespace oid space covers before it would
/// run into whatever comes next. Nothing allocates above it today; the
/// bound exists so `schema_name_for_oid` cannot claim an unrelated oid.
pub(crate) const TEMP_NS_OID_SPAN: i64 = 100_000;

/// The namespace oid a relation named `name` belongs to: its session's
/// temporary one when the name carries a temp prefix, `public` otherwise.
#[must_use]
pub(crate) fn namespace_oid_for_relname(cat: &Catalog, name: &str) -> i64 {
    // 9.0.0 (C9) — a relation key carries the schema it belongs to.
    let (schema, bare) = spg_sql::namespace::split_key(name);
    if let Some(sid) = crate::Engine::temp_session_of(bare) {
        return TEMP_NS_OID_BASE + i64::from(sid);
    }
    user_schema_oid(cat, schema).unwrap_or(2200)
}

/// 9.0.0 (C9) — the oid a `CREATE SCHEMA` schema takes, or `None` for
/// `public` and the built-ins, which have PostgreSQL's own oids.
///
/// Positional inside the registry, the way every other oid this catalog
/// hands out is positional: what a client can rely on is that
/// `relnamespace` joins `pg_namespace` and that `::regnamespace` names
/// the same schema, which the two functions here settle between them.
#[must_use]
pub(crate) fn user_schema_oid(cat: &Catalog, schema: &str) -> Option<i64> {
    let i = cat.user_schemas().iter().position(|s| s == schema)?;
    i64::try_from(i).ok().map(|i| USER_NS_OID_BASE + i)
}

/// 9.0.0 (C9) — the oid band a `CREATE SCHEMA` schema is numbered in.
/// Above every relation oid this catalog hands out (which start at
/// 16384 and count up with the relations) and below the temp-namespace
/// band, so the three spaces cannot collide.
pub(crate) const USER_NS_OID_BASE: i64 = 700_000;
/// How many schemas the band covers. See [`TEMP_NS_OID_SPAN`].
pub(crate) const USER_NS_OID_SPAN: i64 = 100_000;

/// v7.39 (round 623, S05b) — pg_class's own columns, hoisted for the same
/// reason as [`pg_attribute_schema`]: pg_class is one of the relations
/// pg_class now lists.
fn pg_class_schema() -> Vec<ColumnSchema> {
    alloc::vec![
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("relname", DataType::Name, false),
        ColumnSchema::new("relnamespace", DataType::Oid, false),
        ColumnSchema::new("reltype", DataType::Oid, false),
        ColumnSchema::new("reloftype", DataType::Oid, false),
        ColumnSchema::new("relowner", DataType::Oid, false),
        ColumnSchema::new("relam", DataType::Oid, false),
        ColumnSchema::new("relfilenode", DataType::Oid, false),
        ColumnSchema::new("reltablespace", DataType::Oid, false),
        ColumnSchema::new("relpages", DataType::Int, false),
        ColumnSchema::new("reltuples", DataType::Real, false),
        ColumnSchema::new("relallvisible", DataType::Int, false),
        ColumnSchema::new("reltoastrelid", DataType::Oid, false),
        ColumnSchema::new("relhasindex", DataType::Bool, false),
        ColumnSchema::new("relisshared", DataType::Bool, false),
        ColumnSchema::new("relpersistence", DataType::Char1, false),
        ColumnSchema::new("relkind", DataType::Char1, false),
        ColumnSchema::new("relnatts", DataType::SmallInt, false),
        ColumnSchema::new("relchecks", DataType::SmallInt, false),
        ColumnSchema::new("relhasrules", DataType::Bool, false),
        ColumnSchema::new("relhastriggers", DataType::Bool, false),
        ColumnSchema::new("relhassubclass", DataType::Bool, false),
        ColumnSchema::new("relrowsecurity", DataType::Bool, false),
        ColumnSchema::new("relforcerowsecurity", DataType::Bool, false),
        ColumnSchema::new("relispopulated", DataType::Bool, false),
        ColumnSchema::new("relreplident", DataType::Char1, false),
        ColumnSchema::new("relispartition", DataType::Bool, false),
        // v7.39 (read01 round 57) — PG leaves relacl NULL while only the
        // owner's implicit privileges apply, and materialises the aclitem
        // array on the first GRANT. SPG now does the same for real (round 51
        // hard-coded the NULL, because GRANT was a no-op).
        ColumnSchema::new("relacl", DataType::AclItemArray, true),
    ]
}

pub(crate) fn synth_pg_class(
    cat: &Catalog,
    frozen_xid: i64,
    roles: &crate::role_directory::RoleDirectory,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    use spg_storage::PartitionRole;
    let schema = pg_class_schema();
    // v7.39 (round 541) — the six PG18 columns pg_class did not publish.
    //
    // `pg_dump`'s main relation query selects relfrozenxid, relminmxid
    // and reloptions by name and stopped on the first one missing. The
    // six are spliced in ONCE, at PG's own positions, rather than
    // threaded through each of the four row-building loops below —
    // twelve positional insertions into untyped value lists is how a
    // column ends up one slot out.
    let mut schema = schema;
    splice_pg_class_v18_schema(&mut schema);
    let mut rows: Vec<Row<'static>> = Vec::new();
    // relname -> reloptions, for the views that carry a check option.
    // PG stores it there and pg_dump reads it back out to decide
    // whether to write WITH LOCAL/CASCADED CHECK OPTION.
    let mut view_reloptions: alloc::collections::BTreeMap<alloc::string::String, &'static str> =
        alloc::collections::BTreeMap::new();
    // v7.39 (round 642) — `relhassubclass` was hardcoded false, so a
    // PARTITIONED parent reported that it had no children while
    // `pg_inherits` listed them two queries away. PG reads true for a
    // partitioned parent and for an inheritance parent alike, measured.
    // Collected once for the whole scan rather than asked per row: the
    // question is "does any relation name this one as its parent", and
    // answering it inside the loop would walk the catalog per relation.
    let parents_with_children: alloc::collections::BTreeSet<alloc::string::String> = cat
        .visible_relation_keys()
        .iter()
        .filter_map(|n| cat.get(n))
        .flat_map(|c| match &c.schema().partition_role {
            Some(PartitionRole::Range { parent_name, .. })
            | Some(PartitionRole::List { parent_name, .. })
            | Some(PartitionRole::Hash { parent_name, .. })
            | Some(PartitionRole::Default { parent_name }) => {
                alloc::vec![parent_name.to_ascii_lowercase()]
            }
            // v7.39 (round 645) — an inheritance child makes every
            // parent it names a parent.
            Some(PartitionRole::Inherits { parent_names }) => parent_names
                .iter()
                .map(|p| p.to_ascii_lowercase())
                .collect::<alloc::vec::Vec<_>>(),
            _ => alloc::vec::Vec::new(),
        })
        .collect();
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
        let Some(t) = cat.get(&stored) else { continue };
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
        let reltuples = t.rows().len() as f32;
        // relpages in PG-page units (8 KiB) off the maintained
        // hot-tier byte meter — capacity queries multiply
        // relpages × 8192 to estimate table size.
        let relpages = i32::try_from(t.hot_bytes().div_ceil(8192)).unwrap_or(i32::MAX);
        // 9.0.0 — a partitioned parent holds no storage index, and its
        // index DECLARATIONS are what `pg_dump` looks for once this says
        // the relation has any. It read `false`, so the declaration was
        // never written and a restore lost it.
        let has_index = !t.indices().is_empty()
            || matches!(
                &schema_ref.partition_role,
                Some(PartitionRole::Parent {
                    index_template_sources,
                    ..
                }) if !index_template_sources.is_empty()
            );
        let has_triggers = cat
            .triggers()
            .iter()
            .any(|tr| tr.table.eq_ignore_ascii_case(&tname));
        let has_checks = schema_ref.checks.len();
        rows.push(Row::new(alloc::vec![
            Value::BigInt(this_oid),
            Value::text(tname.clone()),
            // v7.38.14 — `pg_temp_N` for a session's temporary relation.
            Value::BigInt(namespace_oid_for_relname(cat, &stored)),
            Value::BigInt(0), // reltype (composite type OID; SPG no composite)
            Value::BigInt(0), // reloftype
            Value::BigInt(roles.owner_oid(schema_ref.owner.as_deref())),
            // 8.0.3 — heap (2) for a table and a materialized view, as PG
            // reports; a partitioned parent stores nothing and has none.
            // pg_dump reads it to emit `SET default_table_access_method`.
            Value::BigInt(if relkind == "p" { 0 } else { 2 }),
            Value::BigInt(this_oid), // relfilenode shares oid in SPG (no separate fork)
            Value::BigInt(0),        // reltablespace (0 == default)
            Value::Int(relpages),    // hot_bytes in 8 KiB PG-page units
            Value::Real(reltuples),  // reltuples
            Value::Int(0),           // relallvisible — visibility map lands in 15.17
            Value::BigInt(0),        // reltoastrelid (SPG no TOAST)
            Value::Bool(has_index),
            Value::Bool(false), // relisshared
            // v7.39 (round 526) — 't' for a TEMPORARY relation. This was
            // pinned 'p', so every temp table, view and sequence reported
            // itself permanent: a tool listing temp objects found none,
            // and one deciding what to dump saw them as dumpable. The
            // witness is the one the resolver already uses — a temp
            // relation is stored under a session-prefixed name, so its
            // stored and listed names differ.
            char1(if is_temp { "t" } else { "p" }), // relpersistence
            char1(relkind),
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
            Value::Bool(parents_with_children.contains(&tname.to_ascii_lowercase())),
            Value::Bool(schema_ref.row_security), // relrowsecurity (v7.39 RLS)
            Value::Bool(schema_ref.force_row_security), // relforcerowsecurity
            Value::Bool(true),                    // relispopulated
            char1("d"),                           // relreplident — 'd' default
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
        // v7.39 (round 541) — a view's WITH CHECK OPTION lives in
        // reloptions in PG, and that is where pg_dump reads it back
        // from. SPG stored the option (information_schema.views
        // reported it) but pg_class never published it, so a dump lost
        // the clause.
        if let Some(v) = cat.views_all().get(stored) {
            match v.check_option {
                1 => {
                    view_reloptions
                        .insert(alloc::string::String::from(vname), "check_option=local");
                }
                2 => {
                    view_reloptions
                        .insert(alloc::string::String::from(vname), "check_option=cascaded");
                }
                _ => {}
            }
        }
        rows.push(Row::new(alloc::vec![
            Value::BigInt(view_oid),
            Value::text(vname.to_string()),
            Value::BigInt(namespace_oid_for_relname(cat, stored)),
            Value::BigInt(0), // reltype
            Value::BigInt(0), // reloftype
            Value::BigInt(
                roles.owner_oid(cat.object_owner(spg_storage::NonTableKind::View, stored))
            ),
            Value::BigInt(0), // relam — a view has no access method
            Value::BigInt(0), // relfilenode — nor any storage
            Value::BigInt(0),
            Value::Int(0),     // relpages
            Value::Real(-1.0), // reltuples — -1 = never analysed
            Value::Int(0),
            Value::BigInt(0),
            Value::Bool(false), // relhasindex
            Value::Bool(false),
            char1(if is_temp { "t" } else { "p" }), // relpersistence
            char1("v"),                             // relkind — view
            Value::SmallInt(relnatts),
            Value::SmallInt(0),
            Value::Bool(true),  // relhasrules — the _RETURN rule
            Value::Bool(false), // relhastriggers
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(true),  // relispopulated
            char1("n"),         // relreplident — 'n' for a view
            Value::Bool(false), // relispartition
            Value::Null,        // relacl
        ]));
    }
    // 7.38.1 S5.2 — a row PER COMPOSITE TYPE (relkind 'c'): PG models
    // a composite as a field-only relation, and pg_dump reads the
    // type's FIELDS from pg_attribute via pg_type.typrelid. Without
    // these rows every composite dumped as `CREATE TYPE x AS ()` and
    // its column data refused to restore anywhere. Oid band 56_001+,
    // in composite_types() order (typrelid in synth_pg_type and
    // attrelid in synth_pg_attribute follow the same sequence).
    for (ci, (cname, def)) in cat.composite_types().iter().enumerate() {
        let comp_oid = 56_001 + ci as i64;
        let relnatts = i16::try_from(def.fields.len()).unwrap_or(0);
        rows.push(Row::new(alloc::vec![
            Value::BigInt(comp_oid),
            Value::text(cname.clone()),
            Value::BigInt(namespace_oid_for_relname(cat, cname)),
            Value::BigInt(54_001 + ci as i64), // reltype — the pg_type row
            Value::BigInt(0),                  // reloftype
            Value::BigInt(
                roles.owner_oid(cat.object_owner(spg_storage::NonTableKind::CompositeType, cname),)
            ),
            Value::BigInt(0), // relam
            Value::BigInt(0), // relfilenode
            Value::BigInt(0),
            Value::Int(0),     // relpages
            Value::Real(-1.0), // reltuples
            Value::Int(0),
            Value::BigInt(0),
            Value::Bool(false), // relhasindex
            Value::Bool(false),
            char1("p"), // relpersistence
            char1("c"), // relkind — composite type
            Value::SmallInt(relnatts),
            Value::SmallInt(0),
            Value::Bool(false), // relhasrules
            Value::Bool(false), // relhastriggers
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(true),  // relispopulated
            char1("n"),         // relreplident
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
    for ci in catalog_indexes(cat) {
        {
            let idx_oid = ci.oid;
            let relnatts = i16::try_from(ci.columns.len()).unwrap_or(i16::MAX);
            rows.push(Row::new(alloc::vec![
                Value::BigInt(idx_oid),
                Value::text(ci.name.clone()),
                Value::BigInt(namespace_oid_for_relname(cat, &ci.name)),
                Value::BigInt(0), // reltype (indexes have none)
                Value::BigInt(0), // reloftype
                // An index belongs to its table's owner, as in PG.
                Value::BigInt(
                    roles.owner_oid(cat.get(&ci.table).and_then(|t| t.schema().owner.as_deref()),)
                ),
                Value::BigInt(ci.am_oid), // relam — the real AM
                Value::BigInt(idx_oid),
                Value::BigInt(0),
                Value::Int(0),
                Value::Real(0.0), // reltuples
                Value::Int(0),
                Value::BigInt(0),
                Value::Bool(false), // relhasindex (an index has none)
                Value::Bool(false),
                char1("p"), // relpersistence
                // 9.0.0 — `I` for a partitioned table's index declaration,
                // as PostgreSQL 18.6 reports (measured). `pg_dump` reads
                // this to decide whether to write the index at all: with
                // `i` it took SPG's parent declaration for an ordinary
                // index on a relation that stores nothing and emitted
                // none.
                char1(if ci.only { "I" } else { "i" }), // relkind — index
                Value::SmallInt(relnatts),
                Value::SmallInt(0),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(true),
                char1("n"), // relreplident — 'n' for an index
                Value::Bool(false),
                Value::Null,
            ]));
        }
    }
    // v7.39 (read01 round 60) — and a row PER SEQUENCE (relkind 'S'). Sequences
    // were missing from pg_class entirely, so `SELECT relacl FROM pg_class WHERE
    // relname = '<seq>'` — the canonical way to read a sequence's privileges —
    // came back empty.
    for (stored, def) in crate::sequence::catalog_sequences(cat) {
        let (stored, def) = (stored.as_str(), def.as_ref());
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
            Value::BigInt(namespace_oid_for_relname(cat, stored)),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(roles.owner_oid(sequence_owner(cat, def))),
            Value::BigInt(0), // relam — a sequence has no access method
            Value::BigInt(seq_oid),
            Value::BigInt(0),
            Value::Int(1),    // relpages — a sequence is one page
            Value::Real(1.0), // reltuples — and one tuple
            Value::Int(0),
            Value::BigInt(0),
            Value::Bool(false), // relhasindex
            Value::Bool(false),
            char1(if is_temp { "t" } else { "p" }), // relpersistence
            char1("S"),                             // relkind — SEQUENCE
            Value::SmallInt(3),
            Value::SmallInt(0),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(true),
            char1("n"), // relreplident
            Value::Bool(false),
            crate::acl::render_acl_list(&def.acl).map_or(Value::Null, Value::text),
        ]));
    }
    // v7.39 (round 623, S05b) — the catalogs themselves.
    //
    // `SELECT count(*) FROM pg_class WHERE relname = 'pg_class'` answered 0.
    // SPG's catalogs were invisible to the catalogs, so a tool asking what
    // relations exist saw the user's tables and nothing that would let it
    // ask about the catalogs it was about to query.
    //
    // `relnamespace` is pg_catalog's oid, which is what keeps them OUT of
    // everything that lists user relations: every such query filters on the
    // namespace (pg_dump, psql \dt, and SPG's own pg_tables all do), and a
    // catalog landing in `public` would show up as a table the user owns.
    // relkind 'r' and relpersistence 'p', as PG reports for its own.
    for (name, oid, relkind, _) in CATALOG_RELATIONS {
        let relnatts = catalog_relation_columns(name, cat)
            .map_or(0, |c| i16::try_from(c.len()).unwrap_or(i16::MAX));
        rows.push(Row::new(alloc::vec![
            Value::BigInt(*oid),
            Value::text((*name).to_string()),
            Value::BigInt(11), // relnamespace — pg_catalog
            Value::BigInt(0),  // reltype
            Value::BigInt(0),  // reloftype
            Value::BigInt(10), // relowner
            Value::BigInt(2),  // relam — heap
            Value::BigInt(*oid),
            Value::BigInt(0),
            Value::Int(0),     // relpages
            Value::Real(-1.0), // reltuples — never analysed
            Value::Int(0),
            Value::BigInt(0),
            Value::Bool(false), // relhasindex
            Value::Bool(false), // relisshared
            char1("p"),         // relpersistence
            // 9.0.0 — the catalog says which kind it is: `pg_stats` and
            // the ten views beside it are `v` on PostgreSQL 18.6, and
            // every one of them read `r` here.
            char1(relkind), // relkind
            Value::SmallInt(relnatts),
            Value::SmallInt(0),
            Value::Bool(false), // relhasrules
            Value::Bool(false), // relhastriggers
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(true),  // relispopulated
            char1("n"),         // relreplident
            Value::Bool(false), // relispartition
            Value::Null,        // relacl
        ]));
    }
    for row in &mut rows {
        splice_pg_class_v18_row(row, frozen_xid, &view_reloptions, cat);
    }
    (schema, rows)
}

/// v7.39 (round 541) — where PG18 keeps the six, and what they hold.
///
/// Measured, per relkind:
///
/// ```text
///     table       relfrozenxid <xid>  relminmxid 1
///     sequence    relfrozenxid 0      relminmxid 0
///     index       relfrozenxid 0      relminmxid 0
///     view        relfrozenxid 0      relminmxid 0
/// ```
///
/// — a cutoff exists only where there is heap storage to freeze.
/// relallfrozen and relrewrite read 0 throughout; relpartbound is NULL
/// for everything that is not a partition.
///
/// SPG has no per-relation freeze cutoff, so the value it reports is
/// the database-wide one `pg_database.datfrozenxid` already publishes —
/// read from the same place, so the two cannot drift apart.
const PG_CLASS_RELALLVISIBLE: usize = 11;
const PG_CLASS_RELKIND: usize = 16;
const PG_CLASS_RELISPARTITION: usize = 26;

/// The collation a column of this type carries, as `pg_attribute.attcollation`.
///
/// v7.39 (round 675) — this was hard-coded 0 at both fill sites, under a
/// comment reading "0 (default)". 0 is not the default; it is what PG puts
/// on a type that HAS no collation. Measured on PG18: `text`, `varchar` and
/// `char` carry 100 (the `default` collation), `name` carries 950 (`C`, and
/// PG's name type really is C-collated), and everything else — int, date,
/// bytea, json, uuid, xml, inet, money, numeric, timestamp, bool — is 0.
///
/// SPG's `pg_collation` already lists those three with PG's own oids
/// (default 100, C 950, POSIX 951), so this connects a catalog that was
/// already right to the columns that were reporting nothing.
///
/// A column with an explicit `COLLATE` still reports the type default here.
/// Round 676 measured why, correcting what rounds 670-675 asserted: the
/// parser does capture the clause, into `ColumnDef::collation`, but that is
/// a two-variant MySQL enum (`Binary` / `CaseInsensitive`) and
/// `from_collation_name` folds every name without a `_ci` suffix into
/// `Binary`. `COLLATE "C"`, `"POSIX"` and `"en_US"` all arrive identical.
/// Carrying the name needs a `ColumnSchema` field and a FILE_VERSION
/// appendix — the next step in `docs/COLLATION_RFC.md` §5.
fn pg_attr_collation_named(ty: DataType, declared: Option<&str>) -> i64 {
    // v7.39 (round 676) — an explicit `COLLATE` now answers with the
    // collation it names, using the oids `pg_collation` already publishes.
    // A name SPG cannot perform falls back to the type's collation rather
    // than inventing an oid: F36 records that accepting-and-ignoring is the
    // gap, and reporting a made-up oid would deepen it instead.
    if let Some(name) = declared {
        let n = name.trim();
        if n.eq_ignore_ascii_case("C") {
            return 950;
        }
        if n.eq_ignore_ascii_case("POSIX") {
            return 951;
        }
        if n.eq_ignore_ascii_case("default") {
            return 100;
        }
    }
    pg_attr_collation(ty)
}

fn pg_attr_collation(ty: DataType) -> i64 {
    match ty {
        DataType::Text | DataType::Varchar(_) | DataType::Char(_) => 100,
        DataType::Name => 950,
        _ => 0,
    }
}

fn splice_pg_class_v18_schema(schema: &mut Vec<ColumnSchema>) {
    // v7.39 (round 541) — a REAL text[]. pg_dump does
    // `array_remove(c.reloptions, 'check_option=local')` and
    // `'check_option=local' = ANY (c.reloptions)`; a text column
    // holding `{check_option=local}` LOOKS right when printed and
    // fails both.
    schema.push(ColumnSchema::new("reloptions", DataType::TextArray, true));
    schema.push(ColumnSchema::new(
        "relpartbound",
        DataType::PgNodeTree,
        true,
    ));
    schema.insert(
        PG_CLASS_RELISPARTITION + 1,
        ColumnSchema::new("relminmxid", DataType::Xid, false),
    );
    schema.insert(
        PG_CLASS_RELISPARTITION + 1,
        ColumnSchema::new("relfrozenxid", DataType::Xid, false),
    );
    schema.insert(
        PG_CLASS_RELISPARTITION + 1,
        ColumnSchema::new("relrewrite", DataType::Oid, false),
    );
    schema.insert(
        PG_CLASS_RELALLVISIBLE + 1,
        ColumnSchema::new("relallfrozen", DataType::Int, false),
    );
}

fn splice_pg_class_v18_row(
    row: &mut Row<'static>,
    frozen_xid: i64,
    view_reloptions: &alloc::collections::BTreeMap<alloc::string::String, &'static str>,
    cat: &Catalog,
) {
    // 9.0.0 — relkind is a `"char"`, which is what PG declares it. This
    // read matched `Value::Text` and silently answered "" once the
    // column carried its type, so no relation had a freeze cutoff.
    let relkind = match row.values.get(PG_CLASS_RELKIND) {
        Some(Value::Char1(k)) => alloc::string::String::from(*k as char),
        Some(Value::Text(k)) => k.to_string(),
        _ => alloc::string::String::new(),
    };
    let relname = match row.values.get(1) {
        Some(Value::Text(n)) => n.to_string(),
        _ => alloc::string::String::new(),
    };
    // Only a relation with heap storage has a freeze cutoff.
    let (frozen, minmxid) = if relkind == "r" || relkind == "m" {
        (frozen_xid, 1)
    } else {
        (0, 0)
    };
    let reloptions = view_reloptions
        .get(&relname)
        .filter(|_| relkind == "v" || relkind == "m")
        .map_or(Value::Null, |o| {
            Value::TextArray(alloc::vec![Some(alloc::string::String::from(*o))])
        });
    row.values.push(reloptions);
    // 7.38.1 S5.2 — a partition child's bound deparses here; pg_dump
    // reads it through pg_get_expr and replays ATTACH PARTITION.
    row.values.push(
        cat.get(&relname)
            .and_then(|t| t.schema().partition_role.as_ref())
            .and_then(|role| relpartbound_text(cat, role))
            .map_or(Value::Null, Value::text),
    );
    row.values
        .insert(PG_CLASS_RELISPARTITION + 1, Value::BigInt(minmxid));
    // v7.39 (round 640) — relfrozenxid is an `xid`, and now says so.
    // Typing the column was not enough: the cell has to carry the value
    // identity too, or `age(relfrozenxid)` still reads a bigint and
    // answers "age() needs DATE or TIMESTAMP" — which is exactly what
    // it did, and why round 627's age guard had to be reverted.
    row.values.insert(
        PG_CLASS_RELISPARTITION + 1,
        Value::Xid(u32::try_from(frozen).unwrap_or(u32::MAX)),
    );
    row.values
        .insert(PG_CLASS_RELISPARTITION + 1, Value::BigInt(0)); // relrewrite
    row.values.insert(PG_CLASS_RELALLVISIBLE + 1, Value::Int(0)); // relallfrozen
}

/// v7.16.2 + v7.37.24 (24.8b) — synthesise `pg_catalog.pg_attribute`.
/// Widened from 5 to 16 PG-canonical columns to cover what
/// dashboard / ORM-introspection tools query: column type id +
/// length + nullability + default-presence + identity/generated
/// + array dimensions + collation. Tools doing
/// `SELECT * FROM pg_attribute WHERE attrelid = …::regclass`
/// see the same shape they'd see against PG.
/// v7.39 (round 623, S05b) — the six system columns PG lists in
/// `pg_attribute` for EVERY relation, at negative attnums.
///
/// `attnum < 0` is how a tool tells a system column from a user one, and
/// SPG's pg_attribute had no negative attnums at all — for any relation.
/// PG answers ten rows for `pg_namespace` (four of its own plus these six);
/// SPG answered four.
///
/// Read off PG18 (`WHERE attrelid = 'pg_namespace'::regclass AND attnum < 0`):
/// the numbering is ctid -1, xmin -2, cmin -3, xmax -4, cmax -5, tableoid -6
/// — which is NOT the order `select::SYSTEM_COLUMNS` uses, so it is spelled
/// out rather than derived from it.
const PG_SYSTEM_ATTRIBUTES: &[(&str, i16, i64, i16, bool, &str)] = &[
    // name, attnum, atttypid, attlen, attbyval, attalign
    ("ctid", -1, 27, 6, false, "s"),
    ("xmin", -2, 28, 4, true, "i"),
    ("cmin", -3, 29, 4, true, "i"),
    ("xmax", -4, 28, 4, true, "i"),
    ("cmax", -5, 29, 4, true, "i"),
    ("tableoid", -6, 26, 4, true, "i"),
];

fn push_system_attributes(rows: &mut Vec<Row<'static>>, attrelid: i64) {
    for (name, attnum, typid, attlen, byval, align) in PG_SYSTEM_ATTRIBUTES {
        rows.push(Row::new(alloc::vec![
            Value::BigInt(attrelid),
            Value::text((*name).to_string()),
            Value::BigInt(*typid),
            Value::SmallInt(0), // attstattarget
            Value::SmallInt(*attlen),
            Value::SmallInt(*attnum),
            Value::SmallInt(0), // attndims — smallint, as PG declares it
            Value::Int(-1),
            Value::Bool(*byval),
            char1("p"),         // attstorage
            char1(align),       // attalign
            Value::Bool(true),  // attnotnull — PG marks all six NOT NULL
            Value::Bool(false), // atthasdef
            char1(""),          // attidentity
            char1(""),          // attgenerated
            Value::Bool(false), // attisdropped
            Value::Bool(true),  // attislocal
            Value::SmallInt(0), // attinhcount
            Value::BigInt(0),
            Value::Null,
            char1(""), // attcompression
            Value::Bool(false),
            Value::Null,
            Value::Null,
            Value::Null,
        ]));
    }
}

/// v7.39 (round 623, S05b) — pg_attribute's own columns, hoisted so the
/// relation can describe ITSELF without `catalog_relation_columns` calling
/// back into the synth that calls it.
fn pg_attribute_schema() -> Vec<ColumnSchema> {
    alloc::vec![
        ColumnSchema::new("attrelid", DataType::Oid, false),
        ColumnSchema::new("attname", DataType::Name, false),
        ColumnSchema::new("atttypid", DataType::Oid, false),
        ColumnSchema::new("attstattarget", DataType::SmallInt, false),
        ColumnSchema::new("attlen", DataType::SmallInt, false),
        ColumnSchema::new("attnum", DataType::SmallInt, false),
        // v7.39.11 — PG declares this `smallint`.
        ColumnSchema::new("attndims", DataType::SmallInt, false),
        ColumnSchema::new("atttypmod", DataType::Int, false),
        ColumnSchema::new("attbyval", DataType::Bool, false),
        ColumnSchema::new("attstorage", DataType::Char1, false),
        ColumnSchema::new("attalign", DataType::Char1, false),
        ColumnSchema::new("attnotnull", DataType::Bool, false),
        ColumnSchema::new("atthasdef", DataType::Bool, false),
        ColumnSchema::new("attidentity", DataType::Char1, false),
        ColumnSchema::new("attgenerated", DataType::Char1, false),
        ColumnSchema::new("attisdropped", DataType::Bool, false),
        ColumnSchema::new("attislocal", DataType::Bool, false),
        ColumnSchema::new("attinhcount", DataType::SmallInt, false),
        ColumnSchema::new("attcollation", DataType::Oid, false),
        // v7.39 (read01 round 59) — column-level privileges. NULL until a
        // `GRANT SELECT (col)` lands; a column grant never touches relacl.
        ColumnSchema::new("attacl", DataType::AclItemArray, true),
        // v7.39 (round 543) — PG18's tail. They land here rather than at
        // PG's positions because SPG's pg_attribute order already
        // differs from PG's (attstattarget sits fourth where PG keeps it
        // twenty-first); reordering the whole thing is its own change.
        ColumnSchema::new("attcompression", DataType::Char1, false),
        ColumnSchema::new("atthasmissing", DataType::Bool, false),
        ColumnSchema::new("attoptions", DataType::TextArray, true),
        ColumnSchema::new("attfdwoptions", DataType::TextArray, true),
        ColumnSchema::new("attmissingval", DataType::AnyArray, true),
    ]
}

pub(crate) fn synth_pg_attribute(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_attribute_schema();
    let mut rows: Vec<Row<'static>> = Vec::new();
    let mut attrelid: i64 = 16384;
    for tname in cat.visible_relation_keys() {
        // 9.0.0 (C9) — the loop walks KEYS; `tname` is the name a client
        // reads and `nsp` the schema column it belongs in, while every
        // catalog lookup below goes on `key`.
        let key = tname;
        let nsp = alloc::string::String::from(cat.listed_schema(&key));
        let Some(tname) = cat.listed_name(&key).map(alloc::string::String::from) else {
            continue;
        };
        let Some(t) = cat.get(&key) else {
            attrelid = attrelid.saturating_add(1);
            continue;
        };
        for (i, col) in t.schema().columns.iter().enumerate() {
            #[allow(clippy::cast_possible_wrap)]
            let attnum = (i + 1) as i16;
            // PG: typlen — fixed-width width in bytes; -1 for var-length.
            // 8.0.3 — the length of the column's catalogued TYPE, a user
            // type's included: an enum column stored as text read
            // `attstorage 'x'` against its type's `'p'`, and `pg_dump` wrote
            // `SET STORAGE EXTENDED` for it.
            let atttypid = attr_type_oid(cat, col);
            let typlen: i16 = type_len_of_oid(cat, atttypid);
            // attndims — number of array dimensions. Most array
            // types are 1-D in SPG; jagged / 2-D arrays report 2.
            //
            // v7.39.11 — `smallint`, which is what PostgreSQL declares
            // it. Reported by sentori against 7.39.10, where it was
            // `integer`: a client that binds the column by its declared
            // width reads two bytes and gets the wrong number.
            // v7.40.0 — one table, not two. The list here used to be a
            // shorter copy of `array_element_type`'s and answered 0 for
            // six array spellings PG 18.6 answers 1 for.
            let attndims: i16 = match col.ty {
                DataType::IntArray2D | DataType::BigIntArray2D | DataType::TextArray2D => 2,
                other => i16::from(crate::conversions::array_element_type(other).is_some()),
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
            let has_default =
                col.default.is_some() || col.runtime_default.is_some() || is_serial(col);
            // attidentity — '' (none), 'a' ALWAYS, 'd' BY DEFAULT.
            //
            // 8.0.3 — SPG records one auto-increment flag for SERIAL and
            // for `GENERATED BY DEFAULT AS IDENTITY` alike, and cannot tell
            // them apart (a field that could is a breaking change to
            // ColumnSchema). It reported every one as BY DEFAULT identity,
            // with no sequence edge, so `pg_dump` wrote neither the
            // identity nor a default and the column restored as a plain
            // integer. It is reported as SERIAL now — a default and an
            // owned sequence, the spelling every schema this project's
            // corpora carry uses — and ALWAYS, which is recorded, as 'a'.
            // 9.0.0 — and 'd' for BY DEFAULT, which read as the empty
            // string (PostgreSQL 18.6's answer for "not an identity
            // column at all").
            let attidentity = match col.identity {
                Some(k) if col.auto_increment => k.attidentity(),
                _ => "",
            };
            rows.push(Row::new(alloc::vec![
                Value::BigInt(attrelid),
                Value::text(col.name.clone()),
                Value::BigInt(attr_type_oid(cat, col)),
                Value::SmallInt(-1), // attstattarget — -1 = use system default
                Value::SmallInt(typlen),
                Value::SmallInt(attnum),
                Value::SmallInt(attndims),
                Value::Int(pg_atttypmod(col.ty)),
                Value::Bool(typlen > 0 && typlen <= 8),
                char1(attstorage),
                char1(attalign),
                Value::Bool(!col.nullable),
                Value::Bool(has_default),
                char1(attidentity),
                // v7.38 (read01 P6.41) — 's' for a STORED generated column
                // (SPG stores generated columns as STORED), '' otherwise.
                char1(if col.generated_stored_expr.is_some() {
                    "s"
                } else {
                    ""
                }),
                Value::Bool(false), // attisdropped
                Value::Bool(true),  // attislocal — true (not inherited)
                Value::SmallInt(0), // attinhcount
                // 8.0.3 — the TYPE's collation unless one was declared: an
                // enum column is not collatable, and reporting text's 100
                // made `pg_dump` write `COLLATE pg_catalog."default"` on it,
                // which PostgreSQL refuses (`collations are not supported
                // by type`).
                Value::BigInt(match col.collation_name.as_deref() {
                    Some(declared) => pg_attr_collation_named(col.ty, Some(declared)),
                    None => type_collation_of_oid(cat, atttypid),
                }), // attcollation
                crate::acl::render_acl_list(&col.acl).map_or(Value::Null, Value::text),
                // v7.39 (round 543) — PG18's tail, measured on a plain
                // table: attcompression empty, atthasmissing false,
                // the rest NULL.
                char1(""),          // attcompression
                Value::Bool(false), // atthasmissing
                Value::Null,        // attoptions
                Value::Null,        // attfdwoptions
                Value::Null,        // attmissingval
            ]));
        }
        push_system_attributes(&mut rows, attrelid);
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
                Value::BigInt(attr_type_oid(cat, col)),
                Value::SmallInt(-1), // attstattarget
                Value::SmallInt(typlen),
                Value::SmallInt(attnum),
                Value::SmallInt(0), // attndims — smallint, as PG declares it
                Value::Int(-1),
                Value::Bool(typlen > 0 && typlen <= 8),
                char1(if typlen > 0 { "p" } else { "x" }), // attstorage
                char1(match typlen {
                    1 => "c",
                    2 => "s",
                    4 => "i",
                    _ => "d",
                }),
                // A view column is nullable regardless of the base
                // column's constraint — an outer join can null it.
                Value::Bool(false),
                Value::Bool(false), // atthasdef
                char1(""),          // attidentity
                char1(""),          // attgenerated
                Value::Bool(false), // attisdropped
                Value::Bool(true),  // attislocal
                Value::SmallInt(0), // attinhcount
                Value::BigInt(0),
                Value::Null, // attacl
                // v7.39 (round 543) — PG18's tail, measured on a plain
                // table: attcompression empty, atthasmissing false,
                // the rest NULL.
                char1(""),          // attcompression
                Value::Bool(false), // atthasmissing
                Value::Null,        // attoptions
                Value::Null,        // attfdwoptions
                Value::Null,        // attmissingval
            ]));
        }
    }
    // 7.38.1 S5.2 — the fields of each COMPOSITE TYPE, keyed to the
    // relkind-'c' pg_class rows (oid band 56_001+): pg_dump reads
    // `CREATE TYPE x AS (…)` field lists from here via typrelid.
    for (ci, (_cname, def)) in cat.composite_types().iter().enumerate() {
        let comp_oid = 56_001 + ci as i64;
        for (i, (fname, fty)) in def.fields.iter().enumerate() {
            #[allow(clippy::cast_possible_wrap)]
            let attnum = (i + 1) as i16;
            let typlen: i16 = pg_type_len(*fty);
            // A field that is itself a named user type carries that
            // type's oid, same rule as attr_type_oid for columns.
            let type_oid = def
                .field_user_types
                .get(i)
                .and_then(|u| u.as_ref())
                .and_then(|uname| {
                    let (enums, composites, domains) = user_type_oids(cat);
                    enums
                        .iter()
                        .chain(composites.iter())
                        .chain(domains.iter())
                        .find(|(n, _)| n == uname)
                        .map(|(_, o)| *o)
                })
                .unwrap_or_else(|| pg_type_oid(*fty));
            rows.push(Row::new(alloc::vec![
                Value::BigInt(comp_oid),
                Value::text(fname.clone()),
                Value::BigInt(type_oid),
                Value::SmallInt(-1), // attstattarget
                Value::SmallInt(typlen),
                Value::SmallInt(attnum),
                Value::SmallInt(0), // attndims — smallint, as PG declares it
                Value::Int(pg_atttypmod(*fty)),
                Value::Bool(typlen > 0 && typlen <= 8),
                char1(if typlen > 0 { "p" } else { "x" }), // attstorage
                char1(match typlen {
                    1 => "c",
                    2 => "s",
                    4 => "i",
                    _ => "d",
                }),
                Value::Bool(false), // attnotnull
                Value::Bool(false), // atthasdef
                char1(""),          // attidentity
                char1(""),          // attgenerated
                Value::Bool(false), // attisdropped
                Value::Bool(true),  // attislocal
                Value::SmallInt(0), // attinhcount
                Value::BigInt(0),   // attcollation
                Value::Null,        // attacl
                char1(""),          // attcompression
                Value::Bool(false), // atthasmissing
                Value::Null,        // attoptions
                Value::Null,        // attfdwoptions
                Value::Null,        // attmissingval
            ]));
        }
    }
    // v7.39 (round 623, S05b) — the catalogs' OWN columns.
    //
    // This loop walked the user's relations only, so `pg_attribute` could
    // not answer what columns `pg_class` has — the question a tool asks
    // BEFORE it queries pg_class. PG has 2584 such rows; SPG had none.
    //
    // The types are what the relation actually publishes, read off the
    // synth's schema, so a column added to a catalog shows up here without
    // anyone remembering to. `attnotnull` follows the schema's own
    // nullability; the rest is what a plain column reports.
    for (name, oid, _, _) in CATALOG_RELATIONS {
        let Some(cols) = catalog_relation_columns(name, cat) else {
            continue;
        };
        for (i, col) in cols.iter().enumerate() {
            #[allow(clippy::cast_possible_wrap)]
            let attnum = (i + 1) as i16;
            let typlen: i16 = pg_type_len(col.ty);
            rows.push(Row::new(alloc::vec![
                Value::BigInt(*oid),
                Value::text(col.name.clone()),
                Value::BigInt(attr_type_oid(cat, col)),
                Value::SmallInt(-1), // attstattarget
                Value::SmallInt(typlen),
                Value::SmallInt(attnum),
                Value::SmallInt(0), // attndims — smallint, as PG declares it
                Value::Int(pg_atttypmod(col.ty)),
                Value::Bool(typlen > 0 && typlen <= 8),
                char1(if typlen > 0 { "p" } else { "x" }), // attstorage
                char1("i"),                                // attalign
                Value::Bool(!col.nullable),
                Value::Bool(false),                       // atthasdef
                char1(""),                                // attidentity
                char1(""),                                // attgenerated
                Value::Bool(false),                       // attisdropped
                Value::Bool(true),                        // attislocal
                Value::SmallInt(0),                       // attinhcount
                Value::BigInt(pg_attr_collation(col.ty)), // attcollation
                Value::Null,                              // attacl
                char1(""),                                // attcompression
                Value::Bool(false),                       // atthasmissing
                Value::Null,                              // attoptions
                Value::Null,                              // attfdwoptions
                Value::Null,                              // attmissingval
            ]));
        }
        push_system_attributes(&mut rows, *oid);
    }
    (schema, rows)
}

/// v7.39 (round 338) — PG's typlen for a type: the fixed width in bytes,
/// -1 for a var-length one. Shared by the table and view attribute rows.
/// 8.0.3 — a column's `attlen`: its type's `typlen`, from the one table
/// `pg_type` publishes. This was a second, shorter table that knew seven
/// types, so a `uuid` column read `attlen -1` and `attstorage 'x'` while
/// its type read 16 and `'p'`, and `pg_dump` wrote
/// `ALTER COLUMN … SET STORAGE EXTENDED` for every uuid column.
fn pg_type_len(ty: DataType) -> i16 {
    let oid = pg_type_oid(ty);
    PG_SCALAR_TYPES
        .iter()
        .find(|t| t.0 == oid)
        .map_or(-1, |t| t.2)
}

/// `(oid, name, len, type, cat, elem, array_oid)` of the built-in scalar
/// types. PG OID numbers come straight from `pg_type.dat`.
const PG_SCALAR_TYPES: &[(i64, &str, i16, &str, &str, i64, i64)] = &[
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
    // v7.39 (round 640) — the four types the row header is made of.
    // `pg_attribute` has described `ctid` / `xmin` / `cmin` / `xmax`
    // / `cmax` since round 623 and typed them 27 / 28 / 29, but
    // `pg_type` listed none of the three: 120 of SPG's own catalog
    // rows pointed at types nothing carried, and PG has exactly one
    // such row (a dropped column, atttypid 0). xid8 joins them
    // because `NULL::xid8` now resolves to a type of its own.
    // Measured off PG18; typarray is 0 rather than PG's 1010 / 1011
    // / 1012 / 271 — see the typarray note in `build_row`.
    (27, "tid", 6, "b", "U", 0, 0),
    (28, "xid", 4, "b", "U", 0, 0),
    (29, "cid", 4, "b", "U", 0, 0),
    (5069, "xid8", 8, "b", "U", 0, 0),
    // 9.0.0 — `pg_node_tree` is the type every catalog column that
    // holds a stored expression is declared as (`pg_index.indexprs`,
    // `pg_attrdef.adbin`, `pg_constraint.conbin`, `pg_policy.polqual`
    // and nine more). Declaring them without listing the type left
    // twelve `pg_attribute` rows naming a type `pg_type` did not
    // carry — the same defect round 640 closed for `xid`. Measured
    // off PG18: category Z (internal), no array type.
    (194, "pg_node_tree", -1, "b", "Z", 0, 0),
    // 9.0.0 — the three pseudo-types the I/O functions return. Without
    // them `pg_proc JOIN pg_type ON prorettype` left 64 rows orphaned —
    // a pointer at a type nothing carries, which is the invariant
    // round 640 closed for `xid` and round 638's pin holds. `typarray`
    // is 0 rather than PG's 1263 for `cstring`, as the note in
    // `build_row` explains for the rows above.
    (2275, "cstring", -2, "p", "P", 0, 0),
    // 9.0.0 — `regtype[]`, which `pg_prepared_statements.parameter_types`
    // is declared as. Measured off PG 18.6: element 2206, no array type
    // of its own, the generic array I/O.
    (2211, "_regtype", -1, "b", "A", 2206, 0),
    (269, "table_am_handler", 4, "p", "P", 0, 0),
    (325, "index_am_handler", 4, "p", "P", 0, 0),
    // v7.39.11 — PG's catalog vectors. `pg_index.indkey` and kin are
    // typed as these now, and a column whose `atttypid` names a type
    // `pg_type` does not carry is exactly the defect round 640
    // closed for `xid`: `format_type` answers `???` and every
    // `pg_attribute JOIN pg_type` loses the row. Measured off PG18;
    // typarray is 0 rather than PG's 1006 / 1013, as the note in
    // `build_row` explains for the rows above.
    (22, "int2vector", -1, "b", "A", 21, 0),
    (30, "oidvector", -1, "b", "A", 26, 0),
    (114, "json", -1, "b", "U", 0, 199),
    (142, "xml", -1, "b", "U", 0, 143),
    (700, "float4", 4, "b", "N", 0, 1021),
    (701, "float8", 8, "b", "N", 0, 1022),
    (650, "cidr", -1, "b", "I", 0, 651),
    (869, "inet", -1, "b", "I", 0, 1041),
    // v7.39 (round 635) — the bit-string types. SPG has had the VALUES
    // since the bit family shipped; pg_type never listed the TYPES, so
    // the canonical `pg_cast JOIN pg_type` lost its eight bit rows to
    // the join even though the cast table carried them. Read off PG18:
    // category V, variable length, array oids 1561 / 1563.
    // v7.39 (round 638) — the types pg_proc's rows point at. Publishing
    // 283 more functions left 18 of them orphaned by
    // `pg_proc JOIN pg_type`, and four of those orphans predate this
    // round: array_agg, lag, lead, first_value and last_value have
    // always returned anyelement / anyarray and pg_type never listed
    // either. Read off PG18; the pseudo-types are typtype 'p' and the
    // multiranges 'm', which is how a client tells them apart.
    (2206, "regtype", 4, "b", "N", 0, 0),
    (2249, "record", -1, "p", "P", 0, 0),
    (2277, "anyarray", -1, "p", "P", 0, 0),
    (2283, "anyelement", 4, "p", "P", 0, 0),
    // 9.0.0 (S1) — `anycompatible`, which the three-argument `lag` and
    // `lead` return. Same reason as `anyelement` above: a `pg_proc` row
    // may not point at a type `pg_type` does not carry.
    (5077, "anycompatible", 4, "p", "P", 0, 0),
    (4451, "int4multirange", -1, "m", "R", 0, 0),
    (4532, "nummultirange", -1, "m", "R", 0, 0),
    (4533, "tsmultirange", -1, "m", "R", 0, 0),
    (4534, "tstzmultirange", -1, "m", "R", 0, 0),
    (4535, "datemultirange", -1, "m", "R", 0, 0),
    (4536, "int8multirange", -1, "m", "R", 0, 0),
    (1560, "bit", -1, "b", "V", 0, 0),
    (1562, "varbit", -1, "b", "V", 0, 0),
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
    (3908, "tsrange", -1, "r", "R", 0, 0),
    (3910, "tstzrange", -1, "r", "R", 0, 0),
    (3904, "int4range", -1, "r", "R", 0, 0),
    (3926, "int8range", -1, "r", "R", 0, 0),
    (3906, "numrange", -1, "r", "R", 0, 0),
    (3912, "daterange", -1, "r", "R", 0, 0),
    // v7.39 (round 653) — F20 side-effect, and a good one. Adding the
    // 235 missing pg_proc rows made an existing pin go red: 19 of them
    // named a return type `pg_type` did not carry, so the catalog join
    // dropped them. The pin was right to refuse.
    //
    // Every one of these is a type the engine really produces —
    // measured, not assumed: `point(1,2)` -> `(1,2)`,
    // `'pg_class'::regclass` -> `pg_class`, `acldefault('r',10)` ->
    // `{postgres=arwdDxtm/postgres}`, `range_merge` -> `[1,7)`,
    // `setseed(0.5)` -> void. `regclass` is the sharpest of them: the
    // cast has worked for rounds and the type was never listed.
    // Metadata is PG18's own (oid, typlen, typtype, typcategory,
    // typelem, typarray) — except that `typarray` is ZEROED for the four
    // whose array type SPG does not carry (`_point`, `_macaddr8`,
    // `_regclass`, `_pg_lsn`), which is the convention round 640
    // established for exactly this and which its pin re-enforced the
    // moment these rows landed. `_aclitem` is carried, so `aclitem`
    // keeps its real 1034.
    (600, "point", 16, "b", "G", 701, 0),
    (774, "macaddr8", 8, "b", "U", 0, 0),
    (1033, "aclitem", 16, "b", "U", 0, 1034),
    (1034, "_aclitem", -1, "b", "A", 1033, 0),
    (2205, "regclass", 4, "b", "N", 0, 0),
    (2278, "void", 4, "p", "P", 0, 0),
    (2279, "trigger", 4, "p", "P", 0, 0),
    (3220, "pg_lsn", 8, "b", "U", 0, 0),
    (3831, "anyrange", -1, "p", "P", 0, 0),
    (4537, "anymultirange", -1, "p", "P", 0, 0),
    (5078, "anycompatiblearray", -1, "p", "P", 0, 0),
];

/// PG type OID lookup for the SPG DataType set. Used by
/// `synth_pg_attribute`'s `atttypid` column.
/// v7.39 (round 620) — PG's packed type modifier, which was hard-coded to
/// `-1` (no modifier) for every column. `format_type` reads it to spell the
/// length back out, so `character varying(8)` came out as bare `text` and
/// `numeric(10,2)` as bare `numeric` — the declared precision was simply not
/// reachable through the catalog.
///
/// The packing is PG's, measured against it: a length-carrying character type
/// stores `n + 4` (`varchar(8)` -> 12, `char(4)` -> 8, `char(1)` -> 5), and
/// numeric packs both halves, `((precision << 16) | scale) + 4`
/// (`numeric(10,2)` -> 655366). Everything else has no modifier.
fn pg_atttypmod(ty: DataType) -> i32 {
    match ty {
        DataType::Varchar(n) | DataType::Char(n) => {
            i32::try_from(n).map_or(-1, |n| n.saturating_add(4))
        }
        DataType::Numeric { precision, scale } => {
            let p = i32::from(precision);
            let s = i32::from(scale);
            ((p << 16) | s).saturating_add(4)
        }
        _ => -1,
    }
}

/// 7.38.1 S5.2 — a column's `pg_attribute.atttypid`, honouring USER
/// types: a composite / enum / domain column carries the user type's
/// own oid (the bands `user_type_oids` publishes), not the storage
/// representation's builtin oid. `format_type` then names `addr`
/// instead of `jsonb`, which is what pg_dump writes back into
/// CREATE TABLE — a composite column restored as jsonb refuses its
/// own COPY data.
pub(crate) fn attr_type_oid(cat: &Catalog, col: &ColumnSchema) -> i64 {
    let (enums, composites, domains) = user_type_oids(cat);
    if let Some(n) = &col.user_composite_type
        && let Some((_, oid)) = composites.iter().find(|(name, _)| name == n)
    {
        return *oid;
    }
    if let Some(n) = &col.user_enum_type
        && let Some((_, oid)) = enums.iter().find(|(name, _)| name == n)
    {
        return *oid;
    }
    if let Some(n) = &col.user_domain_type
        && let Some((_, oid)) = domains.iter().find(|(name, _)| name == n)
    {
        return *oid;
    }
    pg_type_oid(col.ty)
}

pub(crate) fn pg_type_oid(ty: DataType) -> i64 {
    match ty {
        DataType::Bool => 16,
        DataType::Bytes => 17,
        // v7.39 (round 291) — PG's identifier type has its own OID; the
        // catch-all mapped it to text (25) and `format_type` then had
        // no way back to the name.
        DataType::Name => 19,
        // v7.39 (round 640) — without these two, `pg_attribute.atttypid`
        // read 0 for an `xid` column and `format_type` answered `???`.
        DataType::Xid => 28,
        DataType::Xid8 => 5069,
        DataType::Tid => 27,
        DataType::Cid => 29,
        // 9.0.0 — the wire's own map had `oid` and this one did not, so a
        // catalog column declared `oid` reported `atttypid` 0 and
        // `format_type` answered `???`. Two maps, one question.
        DataType::Oid => 26,
        DataType::RegClass => 2205,
        DataType::RegType => 2206,
        DataType::RegProc => 24,
        DataType::PgNodeTree => 194,
        DataType::AnyArray => 2277,
        DataType::AclItemArray => 1034,
        DataType::Char1Array => 1002,
        DataType::RegTypeArray => 2211,
        // 9.0.0 — PG's internal single-byte type. Without it every
        // catalog column declared `"char"` reported atttypid 0, so 28
        // pg_attribute rows pointed at no pg_type row at all.
        DataType::Char1 => 18,
        // v7.39.11 — PG's own OIDs for the catalog vectors.
        DataType::Int2Vector => 22,
        DataType::OidVector => 30,
        DataType::SmallInt => 21,
        DataType::Int => 23,
        DataType::BigInt => 20,
        DataType::Text => 25,
        // v7.39 (round 620) — a declared `varchar(n)` / `char(n)` reported
        // itself as plain text HERE while `information_schema.columns` (fixed
        // in round 248) reported it correctly two queries away. Both OIDs have
        // been in `pg_type` all along. SPG stores these as text with a length
        // limit, but the column WAS declared as what it was declared as, and
        // that is what introspection — and anything that regenerates DDL from
        // it — has to be told.
        DataType::Varchar(_) => 1043,
        DataType::Char(_) => 1042,
        DataType::Float => 701,
        // v7.39 (round 620) — `real` fell into the catch-all and every REAL
        // column's `pg_attribute.atttypid` was 0. A zero type OID does not
        // join to `pg_type`, so the column DISAPPEARED from the standard
        // introspection query, and `format_type` had nothing to name it with
        // and answered `???`. float4's OID has been in `pg_type` all along;
        // only the column-to-OID direction was missing it.
        DataType::Real => 700,
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
        DataType::OidArray => 1028,
        DataType::SmallIntArray => 1005,
        DataType::FloatArray => 1022,
        DataType::BoolArray => 1000,
        DataType::DateArray => 1182,
        DataType::TimestampArray => 1115,
        DataType::TimestamptzArray => 1185,
        DataType::RealArray => 1021,
        DataType::TimeArray => 1183,
        DataType::TimeTzArray => 1270,
        DataType::InetArray => 1041,
        DataType::XmlArray => 143,
        DataType::UuidArray => 2951,
        DataType::BytesArray => 1001,
        DataType::NumericArray => 1231,
        DataType::JsonArray => 199,
        // 2-D arrays use the same element-array OID as 1-D —
        // PG distinguishes dimensions via attndims, not OID.
        DataType::IntArray2D => 1007,
        DataType::BigIntArray2D => 1016,
        DataType::TextArray2D => 1009,
        // v7.38.7 — the network / money / time-of-day family, which this
        // table never had.
        //
        // `_ => 0` is not a harmless default here: this function is where
        // `pg_attribute.atttypid` comes from, so an `inet` column has been
        // reporting type OID 0 to anything that reflects on the schema,
        // and `format_type` has no way back from 0 to a name. It surfaced
        // as a scalar subquery over a uuid refusing to materialise —
        // sentori has 27 tables whose primary key is one — because that
        // path asks this function what type it is holding.
        //
        // Every OID below is PG's own, and each has a matching entry in
        // `regtype_oid_to_name`, so the round trip closes.
        DataType::Money => 790,
        DataType::Macaddr => 829,
        DataType::Macaddr8 => 774,
        DataType::Inet => 869,
        DataType::Cidr => 650,
        DataType::TsVector => 3614,
        DataType::TsQuery => 3615,
        _ => 0,
    }
}

/// v7.39 (round 621) — the array types, as `(array oid, typname, element oid)`.
///
/// Hoisted out of `synth_pg_type` because `::regtype` needs the same knowledge
/// and did not have it: `1007::regtype` rendered `1007` instead of
/// `integer[]`, and `'integer[]'::regtype` was refused outright, while
/// `format_type(1007,-1)` — a third place that knows — answered correctly. One
/// table, three readers.
/// v7.39 (round 621) — the OID a user-defined type gets, in one place.
///
/// `pg_enum` assigned enum OIDs by counting from 50_000 in catalog order, and
/// `pg_type` did not list user types at all — so `pg_enum JOIN pg_type` came
/// back empty and `SELECT typname FROM pg_type WHERE typtype = 'e'` found
/// nothing, for a type that casts and reports itself correctly everywhere
/// else. Deriving the same OIDs twice by iteration order is how those two
/// would drift apart again, so both read this.
///
/// The bands are disjoint by construction: enums from 50_001, composites from
/// 54_001, domains from 58_001, and `pg_enum`'s per-label OIDs from 60_001.
/// The OID of a domain's base type, for `pg_type.typbasetype`.
fn pg_type_oid_for_domain_base(d: &spg_storage::DomainDef) -> Option<i64> {
    let oid = pg_type_oid(d.base_type);
    (oid != 0).then_some(oid)
}

pub(crate) fn user_type_oids(
    cat: &Catalog,
) -> (
    alloc::vec::Vec<(alloc::string::String, i64)>,
    alloc::vec::Vec<(alloc::string::String, i64)>,
    alloc::vec::Vec<(alloc::string::String, i64)>,
) {
    let enums = cat
        .enum_types()
        .keys()
        .enumerate()
        .map(|(i, n)| (n.clone(), 50_001 + i as i64))
        .collect();
    let composites = cat
        .composite_types()
        .keys()
        .enumerate()
        .map(|(i, n)| (n.clone(), 54_001 + i as i64))
        .collect();
    let domains = cat
        .domain_types()
        .keys()
        .enumerate()
        .map(|(i, n)| (n.clone(), 58_001 + i as i64))
        .collect();
    (enums, composites, domains)
}

pub(crate) const ARRAY_TYPE_OIDS: &[(i64, &str, i64)] = &[
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

/// v7.39 (round 621) — synthesise `pg_catalog.pg_operator`.
///
/// The relation did not exist, so `SELECT … FROM pg_operator` answered
/// `relation "pg_operator" does not exist` — the answer PG gives for a name it
/// has never heard of, for one of its own catalogs. psql's `\do` reads it, and
/// so does anything asking "what does `=` mean between these two types".
///
/// 9.0.0 (N22) — PostgreSQL 18.6's own rows, for every operator SPG
/// EVALUATES.
///
/// Rounds 621 and 639 GENERATED the table: same-type comparisons over
/// one type list, same-type arithmetic over another, oids counting up
/// from 70,000, `oprcode` and the selectivity estimators the literal
/// `-`. It listed 330 rows where PG has 799, and none of its oids,
/// commutators or negators were the ones a client resolves — so
/// `oprcom` and `oprnegate` joined to nothing, and `oprcode` named no
/// function. The rows here are PG's, so all three resolve.
///
/// Which rows to carry was MEASURED, not chosen. Each of PG's 799
/// operators was probed over the wire with NON-NULL operands of its
/// own declared types; the 690 SPG answers are here. The first cut
/// probed with NULLs and came out at 689 different rows, because SPG
/// returns NULL for a NULL operand before it checks the types, so
/// `'a'::bytea ~~ 'b'::bytea` — which answers `LIKE requires text
/// operands` — read as supported. A probe's answer is evidence, not a
/// verdict.
///
/// PostgreSQL itself is the control leg. Twelve probes failed THERE
/// too: two were written wrong, and ten name operators PG carries in
/// the catalog but cannot apply to any literal this probe can spell
/// (`aclinsert` answers "no longer supported"; `record_image_eq` and
/// its five siblings need a named composite; `@@` over `jsonpath`
/// wants a predicate). Those are excluded rather than counted as an
/// SPG gap.
///
/// The polymorphic operand types have no literal of their own, so the
/// probe spells each with a concrete one — `anyarray` as `int4[]`,
/// `anyrange` as `int4range`, `anyenum` as a two-label enum created for
/// the run. Leaving them unspelled is what kept `&&` over arrays, the
/// whole range and multirange family, record comparison and enum
/// comparison — 82 rows — out of the first cut of this table.
///
/// `(oid, name, kind, canmerge, canhash, left, right, result, com, negate, code, rest, join)`
const PG_OPERATORS: &[(
    i64,
    &str,
    &str,
    bool,
    bool,
    i64,
    i64,
    i64,
    i64,
    i64,
    &str,
    &str,
    &str,
)] = &[
    (
        15,
        "=",
        "b",
        true,
        true,
        23,
        20,
        16,
        416,
        36,
        "int48eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        36,
        "<>",
        "b",
        false,
        false,
        23,
        20,
        16,
        417,
        15,
        "int48ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        37,
        "<",
        "b",
        false,
        false,
        23,
        20,
        16,
        419,
        82,
        "int48lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        58,
        "<",
        "b",
        false,
        false,
        16,
        16,
        16,
        59,
        1695,
        "boollt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        59,
        ">",
        "b",
        false,
        false,
        16,
        16,
        16,
        58,
        1694,
        "boolgt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        76,
        ">",
        "b",
        false,
        false,
        23,
        20,
        16,
        418,
        80,
        "int48gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        80,
        "<=",
        "b",
        false,
        false,
        23,
        20,
        16,
        430,
        76,
        "int48le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        82,
        ">=",
        "b",
        false,
        false,
        23,
        20,
        16,
        420,
        37,
        "int48ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        85,
        "<>",
        "b",
        false,
        false,
        16,
        16,
        16,
        85,
        91,
        "boolne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        91,
        "=",
        "b",
        true,
        true,
        16,
        16,
        16,
        91,
        85,
        "booleq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        92,
        "=",
        "b",
        true,
        true,
        18,
        18,
        16,
        92,
        630,
        "chareq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        93,
        "=",
        "b",
        true,
        true,
        19,
        19,
        16,
        93,
        643,
        "nameeq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        94,
        "=",
        "b",
        true,
        true,
        21,
        21,
        16,
        94,
        519,
        "int2eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        95,
        "<",
        "b",
        false,
        false,
        21,
        21,
        16,
        520,
        524,
        "int2lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        96,
        "=",
        "b",
        true,
        true,
        23,
        23,
        16,
        96,
        518,
        "int4eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        97,
        "<",
        "b",
        false,
        false,
        23,
        23,
        16,
        521,
        525,
        "int4lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        98,
        "=",
        "b",
        true,
        true,
        25,
        25,
        16,
        98,
        531,
        "texteq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        254,
        "=",
        "b",
        true,
        true,
        19,
        25,
        16,
        260,
        259,
        "nameeqtext",
        "eqsel",
        "eqjoinsel",
    ),
    (
        255,
        "<",
        "b",
        false,
        false,
        19,
        25,
        16,
        264,
        257,
        "namelttext",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        256,
        "<=",
        "b",
        false,
        false,
        19,
        25,
        16,
        263,
        258,
        "nameletext",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        257,
        ">=",
        "b",
        false,
        false,
        19,
        25,
        16,
        262,
        255,
        "namegetext",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        258,
        ">",
        "b",
        false,
        false,
        19,
        25,
        16,
        261,
        256,
        "namegttext",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        259,
        "<>",
        "b",
        false,
        false,
        19,
        25,
        16,
        265,
        254,
        "namenetext",
        "neqsel",
        "neqjoinsel",
    ),
    (
        260,
        "=",
        "b",
        true,
        true,
        25,
        19,
        16,
        254,
        265,
        "texteqname",
        "eqsel",
        "eqjoinsel",
    ),
    (
        261,
        "<",
        "b",
        false,
        false,
        25,
        19,
        16,
        258,
        263,
        "textltname",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        262,
        "<=",
        "b",
        false,
        false,
        25,
        19,
        16,
        257,
        264,
        "textlename",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        263,
        ">=",
        "b",
        false,
        false,
        25,
        19,
        16,
        256,
        261,
        "textgename",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        264,
        ">",
        "b",
        false,
        false,
        25,
        19,
        16,
        255,
        262,
        "textgtname",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        265,
        "<>",
        "b",
        false,
        false,
        25,
        19,
        16,
        259,
        260,
        "textnename",
        "neqsel",
        "neqjoinsel",
    ),
    (
        349,
        "||",
        "b",
        false,
        false,
        5078,
        5077,
        5078,
        0,
        0,
        "array_append",
        "-",
        "-",
    ),
    (
        352,
        "=",
        "b",
        false,
        true,
        28,
        28,
        16,
        352,
        3315,
        "xideq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        353,
        "=",
        "b",
        false,
        false,
        28,
        23,
        16,
        0,
        3316,
        "xideqint4",
        "eqsel",
        "eqjoinsel",
    ),
    (
        374,
        "||",
        "b",
        false,
        false,
        5077,
        5078,
        5078,
        0,
        0,
        "array_prepend",
        "-",
        "-",
    ),
    (
        375,
        "||",
        "b",
        false,
        false,
        5078,
        5078,
        5078,
        0,
        0,
        "array_cat",
        "-",
        "-",
    ),
    (
        385,
        "=",
        "b",
        false,
        true,
        29,
        29,
        16,
        385,
        0,
        "cideq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        387,
        "=",
        "b",
        true,
        true,
        27,
        27,
        16,
        387,
        402,
        "tideq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        402,
        "<>",
        "b",
        false,
        false,
        27,
        27,
        16,
        402,
        387,
        "tidne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        410,
        "=",
        "b",
        true,
        true,
        20,
        20,
        16,
        410,
        411,
        "int8eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        411,
        "<>",
        "b",
        false,
        false,
        20,
        20,
        16,
        411,
        410,
        "int8ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        412,
        "<",
        "b",
        false,
        false,
        20,
        20,
        16,
        413,
        415,
        "int8lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        413,
        ">",
        "b",
        false,
        false,
        20,
        20,
        16,
        412,
        414,
        "int8gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        414,
        "<=",
        "b",
        false,
        false,
        20,
        20,
        16,
        415,
        413,
        "int8le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        415,
        ">=",
        "b",
        false,
        false,
        20,
        20,
        16,
        414,
        412,
        "int8ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        416,
        "=",
        "b",
        true,
        true,
        20,
        23,
        16,
        15,
        417,
        "int84eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        417,
        "<>",
        "b",
        false,
        false,
        20,
        23,
        16,
        36,
        416,
        "int84ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        418,
        "<",
        "b",
        false,
        false,
        20,
        23,
        16,
        76,
        430,
        "int84lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        419,
        ">",
        "b",
        false,
        false,
        20,
        23,
        16,
        37,
        420,
        "int84gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        420,
        "<=",
        "b",
        false,
        false,
        20,
        23,
        16,
        82,
        419,
        "int84le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        430,
        ">=",
        "b",
        false,
        false,
        20,
        23,
        16,
        80,
        418,
        "int84ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        433,
        "@>",
        "b",
        false,
        false,
        603,
        600,
        16,
        511,
        0,
        "box_contain_pt",
        "contsel",
        "contjoinsel",
    ),
    (
        439, "%", "b", false, false, 20, 20, 20, 0, 0, "int8mod", "-", "-",
    ),
    (
        473, "@", "l", false, false, 0, 20, 20, 0, 0, "int8abs", "-", "-",
    ),
    (
        484, "-", "l", false, false, 0, 20, 20, 0, 0, "int8um", "-", "-",
    ),
    (
        489,
        "<@",
        "b",
        false,
        false,
        604,
        604,
        16,
        490,
        0,
        "poly_contained",
        "contsel",
        "contjoinsel",
    ),
    (
        490,
        "@>",
        "b",
        false,
        false,
        604,
        604,
        16,
        489,
        0,
        "poly_contain",
        "contsel",
        "contjoinsel",
    ),
    (
        491,
        "~=",
        "b",
        false,
        false,
        604,
        604,
        16,
        491,
        0,
        "poly_same",
        "eqsel",
        "eqjoinsel",
    ),
    (
        492,
        "&&",
        "b",
        false,
        false,
        604,
        604,
        16,
        492,
        0,
        "poly_overlap",
        "areasel",
        "areajoinsel",
    ),
    (
        497,
        "<@",
        "b",
        false,
        false,
        603,
        603,
        16,
        498,
        0,
        "box_contained",
        "contsel",
        "contjoinsel",
    ),
    (
        498,
        "@>",
        "b",
        false,
        false,
        603,
        603,
        16,
        497,
        0,
        "box_contain",
        "contsel",
        "contjoinsel",
    ),
    (
        499,
        "~=",
        "b",
        false,
        false,
        603,
        603,
        16,
        499,
        0,
        "box_same",
        "eqsel",
        "eqjoinsel",
    ),
    (
        500,
        "&&",
        "b",
        false,
        false,
        603,
        603,
        16,
        500,
        0,
        "box_overlap",
        "areasel",
        "areajoinsel",
    ),
    (
        501,
        ">=",
        "b",
        false,
        false,
        603,
        603,
        16,
        505,
        504,
        "box_ge",
        "areasel",
        "areajoinsel",
    ),
    (
        502,
        ">",
        "b",
        false,
        false,
        603,
        603,
        16,
        504,
        505,
        "box_gt",
        "areasel",
        "areajoinsel",
    ),
    (
        503,
        "=",
        "b",
        false,
        false,
        603,
        603,
        16,
        503,
        0,
        "box_eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        504,
        "<",
        "b",
        false,
        false,
        603,
        603,
        16,
        502,
        501,
        "box_lt",
        "areasel",
        "areajoinsel",
    ),
    (
        505,
        "<=",
        "b",
        false,
        false,
        603,
        603,
        16,
        501,
        502,
        "box_le",
        "areasel",
        "areajoinsel",
    ),
    (
        506,
        ">^",
        "b",
        false,
        false,
        600,
        600,
        16,
        0,
        0,
        "point_above",
        "positionsel",
        "positionjoinsel",
    ),
    (
        509,
        "<^",
        "b",
        false,
        false,
        600,
        600,
        16,
        0,
        0,
        "point_below",
        "positionsel",
        "positionjoinsel",
    ),
    (
        510,
        "~=",
        "b",
        false,
        false,
        600,
        600,
        16,
        510,
        713,
        "point_eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        511,
        "<@",
        "b",
        false,
        false,
        600,
        603,
        16,
        433,
        0,
        "on_pb",
        "contsel",
        "contjoinsel",
    ),
    (
        512, "<@", "b", false, false, 600, 602, 16, 755, 0, "on_ppath", "-", "-",
    ),
    (
        513,
        "@@",
        "l",
        false,
        false,
        0,
        603,
        600,
        0,
        0,
        "box_center",
        "-",
        "-",
    ),
    (
        514, "*", "b", false, false, 23, 23, 23, 514, 0, "int4mul", "-", "-",
    ),
    (
        517,
        "<->",
        "b",
        false,
        false,
        600,
        600,
        701,
        517,
        0,
        "point_distance",
        "-",
        "-",
    ),
    (
        518,
        "<>",
        "b",
        false,
        false,
        23,
        23,
        16,
        518,
        96,
        "int4ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        519,
        "<>",
        "b",
        false,
        false,
        21,
        21,
        16,
        519,
        94,
        "int2ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        520,
        ">",
        "b",
        false,
        false,
        21,
        21,
        16,
        95,
        522,
        "int2gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        521,
        ">",
        "b",
        false,
        false,
        23,
        23,
        16,
        97,
        523,
        "int4gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        522,
        "<=",
        "b",
        false,
        false,
        21,
        21,
        16,
        524,
        520,
        "int2le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        523,
        "<=",
        "b",
        false,
        false,
        23,
        23,
        16,
        525,
        521,
        "int4le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        524,
        ">=",
        "b",
        false,
        false,
        21,
        21,
        16,
        522,
        95,
        "int2ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        525,
        ">=",
        "b",
        false,
        false,
        23,
        23,
        16,
        523,
        97,
        "int4ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        526, "*", "b", false, false, 21, 21, 21, 526, 0, "int2mul", "-", "-",
    ),
    (
        527, "/", "b", false, false, 21, 21, 21, 0, 0, "int2div", "-", "-",
    ),
    (
        528, "/", "b", false, false, 23, 23, 23, 0, 0, "int4div", "-", "-",
    ),
    (
        529, "%", "b", false, false, 21, 21, 21, 0, 0, "int2mod", "-", "-",
    ),
    (
        530, "%", "b", false, false, 23, 23, 23, 0, 0, "int4mod", "-", "-",
    ),
    (
        531,
        "<>",
        "b",
        false,
        false,
        25,
        25,
        16,
        531,
        98,
        "textne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        532,
        "=",
        "b",
        true,
        true,
        21,
        23,
        16,
        533,
        538,
        "int24eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        533,
        "=",
        "b",
        true,
        true,
        23,
        21,
        16,
        532,
        539,
        "int42eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        534,
        "<",
        "b",
        false,
        false,
        21,
        23,
        16,
        537,
        542,
        "int24lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        535,
        "<",
        "b",
        false,
        false,
        23,
        21,
        16,
        536,
        543,
        "int42lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        536,
        ">",
        "b",
        false,
        false,
        21,
        23,
        16,
        535,
        540,
        "int24gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        537,
        ">",
        "b",
        false,
        false,
        23,
        21,
        16,
        534,
        541,
        "int42gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        538,
        "<>",
        "b",
        false,
        false,
        21,
        23,
        16,
        539,
        532,
        "int24ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        539,
        "<>",
        "b",
        false,
        false,
        23,
        21,
        16,
        538,
        533,
        "int42ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        540,
        "<=",
        "b",
        false,
        false,
        21,
        23,
        16,
        543,
        536,
        "int24le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        541,
        "<=",
        "b",
        false,
        false,
        23,
        21,
        16,
        542,
        537,
        "int42le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        542,
        ">=",
        "b",
        false,
        false,
        21,
        23,
        16,
        541,
        534,
        "int24ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        543,
        ">=",
        "b",
        false,
        false,
        23,
        21,
        16,
        540,
        535,
        "int42ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        544, "*", "b", false, false, 21, 23, 23, 545, 0, "int24mul", "-", "-",
    ),
    (
        545, "*", "b", false, false, 23, 21, 23, 544, 0, "int42mul", "-", "-",
    ),
    (
        546, "/", "b", false, false, 21, 23, 23, 0, 0, "int24div", "-", "-",
    ),
    (
        547, "/", "b", false, false, 23, 21, 23, 0, 0, "int42div", "-", "-",
    ),
    (
        550, "+", "b", false, false, 21, 21, 21, 550, 0, "int2pl", "-", "-",
    ),
    (
        551, "+", "b", false, false, 23, 23, 23, 551, 0, "int4pl", "-", "-",
    ),
    (
        552, "+", "b", false, false, 21, 23, 23, 553, 0, "int24pl", "-", "-",
    ),
    (
        553, "+", "b", false, false, 23, 21, 23, 552, 0, "int42pl", "-", "-",
    ),
    (
        554, "-", "b", false, false, 21, 21, 21, 0, 0, "int2mi", "-", "-",
    ),
    (
        555, "-", "b", false, false, 23, 23, 23, 0, 0, "int4mi", "-", "-",
    ),
    (
        556, "-", "b", false, false, 21, 23, 23, 0, 0, "int24mi", "-", "-",
    ),
    (
        557, "-", "b", false, false, 23, 21, 23, 0, 0, "int42mi", "-", "-",
    ),
    (
        558, "-", "l", false, false, 0, 23, 23, 0, 0, "int4um", "-", "-",
    ),
    (
        559, "-", "l", false, false, 0, 21, 21, 0, 0, "int2um", "-", "-",
    ),
    (
        584, "-", "l", false, false, 0, 700, 700, 0, 0, "float4um", "-", "-",
    ),
    (
        585, "-", "l", false, false, 0, 701, 701, 0, 0, "float8um", "-", "-",
    ),
    (
        586, "+", "b", false, false, 700, 700, 700, 586, 0, "float4pl", "-", "-",
    ),
    (
        587, "-", "b", false, false, 700, 700, 700, 0, 0, "float4mi", "-", "-",
    ),
    (
        588,
        "/",
        "b",
        false,
        false,
        700,
        700,
        700,
        0,
        0,
        "float4div",
        "-",
        "-",
    ),
    (
        589,
        "*",
        "b",
        false,
        false,
        700,
        700,
        700,
        589,
        0,
        "float4mul",
        "-",
        "-",
    ),
    (
        590,
        "@",
        "l",
        false,
        false,
        0,
        700,
        700,
        0,
        0,
        "float4abs",
        "-",
        "-",
    ),
    (
        591, "+", "b", false, false, 701, 701, 701, 591, 0, "float8pl", "-", "-",
    ),
    (
        592, "-", "b", false, false, 701, 701, 701, 0, 0, "float8mi", "-", "-",
    ),
    (
        593,
        "/",
        "b",
        false,
        false,
        701,
        701,
        701,
        0,
        0,
        "float8div",
        "-",
        "-",
    ),
    (
        594,
        "*",
        "b",
        false,
        false,
        701,
        701,
        701,
        594,
        0,
        "float8mul",
        "-",
        "-",
    ),
    (
        595,
        "@",
        "l",
        false,
        false,
        0,
        701,
        701,
        0,
        0,
        "float8abs",
        "-",
        "-",
    ),
    (
        606, "<->", "b", false, false, 603, 600, 701, 615, 0, "dist_bp", "-", "-",
    ),
    (
        607,
        "=",
        "b",
        true,
        true,
        26,
        26,
        16,
        607,
        608,
        "oideq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        608,
        "<>",
        "b",
        false,
        false,
        26,
        26,
        16,
        608,
        607,
        "oidne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        609,
        "<",
        "b",
        false,
        false,
        26,
        26,
        16,
        610,
        612,
        "oidlt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        610,
        ">",
        "b",
        false,
        false,
        26,
        26,
        16,
        609,
        611,
        "oidgt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        611,
        "<=",
        "b",
        false,
        false,
        26,
        26,
        16,
        612,
        610,
        "oidle",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        612,
        ">=",
        "b",
        false,
        false,
        26,
        26,
        16,
        611,
        609,
        "oidge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        613, "<->", "b", false, false, 600, 628, 701, 760, 0, "dist_pl", "-", "-",
    ),
    (
        614, "<->", "b", false, false, 600, 601, 701, 761, 0, "dist_ps", "-", "-",
    ),
    (
        615, "<->", "b", false, false, 600, 603, 701, 606, 0, "dist_pb", "-", "-",
    ),
    (
        617, "<->", "b", false, false, 601, 603, 701, 763, 0, "dist_sb", "-", "-",
    ),
    (
        618,
        "<->",
        "b",
        false,
        false,
        600,
        602,
        701,
        784,
        0,
        "dist_ppath",
        "-",
        "-",
    ),
    (
        620,
        "=",
        "b",
        true,
        true,
        700,
        700,
        16,
        620,
        621,
        "float4eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        621,
        "<>",
        "b",
        false,
        false,
        700,
        700,
        16,
        621,
        620,
        "float4ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        622,
        "<",
        "b",
        false,
        false,
        700,
        700,
        16,
        623,
        625,
        "float4lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        623,
        ">",
        "b",
        false,
        false,
        700,
        700,
        16,
        622,
        624,
        "float4gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        624,
        "<=",
        "b",
        false,
        false,
        700,
        700,
        16,
        625,
        623,
        "float4le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        625,
        ">=",
        "b",
        false,
        false,
        700,
        700,
        16,
        624,
        622,
        "float4ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        630,
        "<>",
        "b",
        false,
        false,
        18,
        18,
        16,
        630,
        92,
        "charne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        631,
        "<",
        "b",
        false,
        false,
        18,
        18,
        16,
        633,
        634,
        "charlt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        632,
        "<=",
        "b",
        false,
        false,
        18,
        18,
        16,
        634,
        633,
        "charle",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        633,
        ">",
        "b",
        false,
        false,
        18,
        18,
        16,
        631,
        632,
        "chargt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        634,
        ">=",
        "b",
        false,
        false,
        18,
        18,
        16,
        632,
        631,
        "charge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        639,
        "~",
        "b",
        false,
        false,
        19,
        25,
        16,
        0,
        640,
        "nameregexeq",
        "regexeqsel",
        "regexeqjoinsel",
    ),
    (
        640,
        "!~",
        "b",
        false,
        false,
        19,
        25,
        16,
        0,
        639,
        "nameregexne",
        "regexnesel",
        "regexnejoinsel",
    ),
    (
        641,
        "~",
        "b",
        false,
        false,
        25,
        25,
        16,
        0,
        642,
        "textregexeq",
        "regexeqsel",
        "regexeqjoinsel",
    ),
    (
        642,
        "!~",
        "b",
        false,
        false,
        25,
        25,
        16,
        0,
        641,
        "textregexne",
        "regexnesel",
        "regexnejoinsel",
    ),
    (
        643,
        "<>",
        "b",
        false,
        false,
        19,
        19,
        16,
        643,
        93,
        "namene",
        "neqsel",
        "neqjoinsel",
    ),
    (
        654, "||", "b", false, false, 25, 25, 25, 0, 0, "textcat", "-", "-",
    ),
    (
        660,
        "<",
        "b",
        false,
        false,
        19,
        19,
        16,
        662,
        663,
        "namelt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        661,
        "<=",
        "b",
        false,
        false,
        19,
        19,
        16,
        663,
        662,
        "namele",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        662,
        ">",
        "b",
        false,
        false,
        19,
        19,
        16,
        660,
        661,
        "namegt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        663,
        ">=",
        "b",
        false,
        false,
        19,
        19,
        16,
        661,
        660,
        "namege",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        664,
        "<",
        "b",
        false,
        false,
        25,
        25,
        16,
        666,
        667,
        "text_lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        665,
        "<=",
        "b",
        false,
        false,
        25,
        25,
        16,
        667,
        666,
        "text_le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        666,
        ">",
        "b",
        false,
        false,
        25,
        25,
        16,
        664,
        665,
        "text_gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        667,
        ">=",
        "b",
        false,
        false,
        25,
        25,
        16,
        665,
        664,
        "text_ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        670,
        "=",
        "b",
        true,
        true,
        701,
        701,
        16,
        670,
        671,
        "float8eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        671,
        "<>",
        "b",
        false,
        false,
        701,
        701,
        16,
        671,
        670,
        "float8ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        672,
        "<",
        "b",
        false,
        false,
        701,
        701,
        16,
        674,
        675,
        "float8lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        673,
        "<=",
        "b",
        false,
        false,
        701,
        701,
        16,
        675,
        674,
        "float8le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        674,
        ">",
        "b",
        false,
        false,
        701,
        701,
        16,
        672,
        673,
        "float8gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        675,
        ">=",
        "b",
        false,
        false,
        701,
        701,
        16,
        673,
        672,
        "float8ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        682, "@", "l", false, false, 0, 21, 21, 0, 0, "int2abs", "-", "-",
    ),
    (
        684, "+", "b", false, false, 20, 20, 20, 684, 0, "int8pl", "-", "-",
    ),
    (
        685, "-", "b", false, false, 20, 20, 20, 0, 0, "int8mi", "-", "-",
    ),
    (
        686, "*", "b", false, false, 20, 20, 20, 686, 0, "int8mul", "-", "-",
    ),
    (
        687, "/", "b", false, false, 20, 20, 20, 0, 0, "int8div", "-", "-",
    ),
    (
        688, "+", "b", false, false, 20, 23, 20, 692, 0, "int84pl", "-", "-",
    ),
    (
        689, "-", "b", false, false, 20, 23, 20, 0, 0, "int84mi", "-", "-",
    ),
    (
        690, "*", "b", false, false, 20, 23, 20, 694, 0, "int84mul", "-", "-",
    ),
    (
        691, "/", "b", false, false, 20, 23, 20, 0, 0, "int84div", "-", "-",
    ),
    (
        692, "+", "b", false, false, 23, 20, 20, 688, 0, "int48pl", "-", "-",
    ),
    (
        693, "-", "b", false, false, 23, 20, 20, 0, 0, "int48mi", "-", "-",
    ),
    (
        694, "*", "b", false, false, 23, 20, 20, 690, 0, "int48mul", "-", "-",
    ),
    (
        695, "/", "b", false, false, 23, 20, 20, 0, 0, "int48div", "-", "-",
    ),
    (
        706,
        "<->",
        "b",
        false,
        false,
        603,
        603,
        701,
        706,
        0,
        "box_distance",
        "-",
        "-",
    ),
    (
        709,
        "<->",
        "b",
        false,
        false,
        601,
        601,
        701,
        709,
        0,
        "lseg_distance",
        "-",
        "-",
    ),
    (
        731,
        "+",
        "b",
        false,
        false,
        600,
        600,
        600,
        731,
        0,
        "point_add",
        "-",
        "-",
    ),
    (
        732,
        "-",
        "b",
        false,
        false,
        600,
        600,
        600,
        0,
        0,
        "point_sub",
        "-",
        "-",
    ),
    (
        733,
        "*",
        "b",
        false,
        false,
        600,
        600,
        600,
        733,
        0,
        "point_mul",
        "-",
        "-",
    ),
    (
        734,
        "/",
        "b",
        false,
        false,
        600,
        600,
        600,
        0,
        0,
        "point_div",
        "-",
        "-",
    ),
    (
        735, "+", "b", false, false, 602, 602, 602, 735, 0, "path_add", "-", "-",
    ),
    (
        738,
        "*",
        "b",
        false,
        false,
        602,
        600,
        602,
        0,
        0,
        "path_mul_pt",
        "-",
        "-",
    ),
    (
        739,
        "/",
        "b",
        false,
        false,
        602,
        600,
        602,
        0,
        0,
        "path_div_pt",
        "-",
        "-",
    ),
    (
        756,
        "<@",
        "b",
        false,
        false,
        600,
        604,
        16,
        757,
        0,
        "pt_contained_poly",
        "contsel",
        "contjoinsel",
    ),
    (
        757,
        "@>",
        "b",
        false,
        false,
        604,
        600,
        16,
        756,
        0,
        "poly_contain_pt",
        "contsel",
        "contjoinsel",
    ),
    (
        758,
        "<@",
        "b",
        false,
        false,
        600,
        718,
        16,
        759,
        0,
        "pt_contained_circle",
        "contsel",
        "contjoinsel",
    ),
    (
        759,
        "@>",
        "b",
        false,
        false,
        718,
        600,
        16,
        758,
        0,
        "circle_contain_pt",
        "contsel",
        "contjoinsel",
    ),
    (
        760, "<->", "b", false, false, 628, 600, 701, 613, 0, "dist_lp", "-", "-",
    ),
    (
        761, "<->", "b", false, false, 601, 600, 701, 614, 0, "dist_sp", "-", "-",
    ),
    (
        763, "<->", "b", false, false, 603, 601, 701, 617, 0, "dist_bs", "-", "-",
    ),
    (
        773, "@", "l", false, false, 0, 23, 23, 0, 0, "int4abs", "-", "-",
    ),
    (
        784,
        "<->",
        "b",
        false,
        false,
        602,
        600,
        701,
        618,
        0,
        "dist_pathp",
        "-",
        "-",
    ),
    (
        797,
        "#",
        "l",
        false,
        false,
        0,
        602,
        23,
        0,
        0,
        "path_npoints",
        "-",
        "-",
    ),
    (
        798,
        "?#",
        "b",
        false,
        false,
        602,
        602,
        16,
        0,
        0,
        "path_inter",
        "-",
        "-",
    ),
    (
        799,
        "@-@",
        "l",
        false,
        false,
        0,
        602,
        701,
        0,
        0,
        "path_length",
        "-",
        "-",
    ),
    (
        800,
        ">^",
        "b",
        false,
        false,
        603,
        603,
        16,
        0,
        0,
        "box_above_eq",
        "positionsel",
        "positionjoinsel",
    ),
    (
        801,
        "<^",
        "b",
        false,
        false,
        603,
        603,
        16,
        0,
        0,
        "box_below_eq",
        "positionsel",
        "positionjoinsel",
    ),
    (
        802,
        "?#",
        "b",
        false,
        false,
        603,
        603,
        16,
        0,
        0,
        "box_overlap",
        "areasel",
        "areajoinsel",
    ),
    (
        803,
        "#",
        "b",
        false,
        false,
        603,
        603,
        603,
        0,
        0,
        "box_intersect",
        "-",
        "-",
    ),
    (
        806, "*", "b", false, false, 603, 600, 603, 0, 0, "box_mul", "-", "-",
    ),
    (
        807, "/", "b", false, false, 603, 600, 603, 0, 0, "box_div", "-", "-",
    ),
    (
        808,
        "?-",
        "b",
        false,
        false,
        600,
        600,
        16,
        808,
        0,
        "point_horiz",
        "-",
        "-",
    ),
    (
        809,
        "?|",
        "b",
        false,
        false,
        600,
        600,
        16,
        809,
        0,
        "point_vert",
        "-",
        "-",
    ),
    (
        818, "+", "b", false, false, 20, 21, 20, 822, 0, "int82pl", "-", "-",
    ),
    (
        819, "-", "b", false, false, 20, 21, 20, 0, 0, "int82mi", "-", "-",
    ),
    (
        820, "*", "b", false, false, 20, 21, 20, 824, 0, "int82mul", "-", "-",
    ),
    (
        821, "/", "b", false, false, 20, 21, 20, 0, 0, "int82div", "-", "-",
    ),
    (
        822, "+", "b", false, false, 21, 20, 20, 818, 0, "int28pl", "-", "-",
    ),
    (
        823, "-", "b", false, false, 21, 20, 20, 0, 0, "int28mi", "-", "-",
    ),
    (
        824, "*", "b", false, false, 21, 20, 20, 820, 0, "int28mul", "-", "-",
    ),
    (
        825, "/", "b", false, false, 21, 20, 20, 0, 0, "int28div", "-", "-",
    ),
    (
        900,
        "=",
        "b",
        true,
        false,
        790,
        790,
        16,
        900,
        901,
        "cash_eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        901,
        "<>",
        "b",
        false,
        false,
        790,
        790,
        16,
        901,
        900,
        "cash_ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        902,
        "<",
        "b",
        false,
        false,
        790,
        790,
        16,
        903,
        905,
        "cash_lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        903,
        ">",
        "b",
        false,
        false,
        790,
        790,
        16,
        902,
        904,
        "cash_gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        904,
        "<=",
        "b",
        false,
        false,
        790,
        790,
        16,
        905,
        903,
        "cash_le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        905,
        ">=",
        "b",
        false,
        false,
        790,
        790,
        16,
        904,
        902,
        "cash_ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        906, "+", "b", false, false, 790, 790, 790, 906, 0, "cash_pl", "-", "-",
    ),
    (
        907, "-", "b", false, false, 790, 790, 790, 0, 0, "cash_mi", "-", "-",
    ),
    (
        908,
        "*",
        "b",
        false,
        false,
        790,
        701,
        790,
        916,
        0,
        "cash_mul_flt8",
        "-",
        "-",
    ),
    (
        909,
        "/",
        "b",
        false,
        false,
        790,
        701,
        790,
        0,
        0,
        "cash_div_flt8",
        "-",
        "-",
    ),
    (
        912,
        "*",
        "b",
        false,
        false,
        790,
        23,
        790,
        917,
        0,
        "cash_mul_int4",
        "-",
        "-",
    ),
    (
        913,
        "/",
        "b",
        false,
        false,
        790,
        23,
        790,
        0,
        0,
        "cash_div_int4",
        "-",
        "-",
    ),
    (
        914,
        "*",
        "b",
        false,
        false,
        790,
        21,
        790,
        918,
        0,
        "cash_mul_int2",
        "-",
        "-",
    ),
    (
        915,
        "/",
        "b",
        false,
        false,
        790,
        21,
        790,
        0,
        0,
        "cash_div_int2",
        "-",
        "-",
    ),
    (
        916,
        "*",
        "b",
        false,
        false,
        701,
        790,
        790,
        908,
        0,
        "flt8_mul_cash",
        "-",
        "-",
    ),
    (
        917,
        "*",
        "b",
        false,
        false,
        23,
        790,
        790,
        912,
        0,
        "int4_mul_cash",
        "-",
        "-",
    ),
    (
        918,
        "*",
        "b",
        false,
        false,
        21,
        790,
        790,
        914,
        0,
        "int2_mul_cash",
        "-",
        "-",
    ),
    (
        931,
        "<<",
        "b",
        false,
        false,
        869,
        869,
        16,
        933,
        0,
        "network_sub",
        "networksel",
        "networkjoinsel",
    ),
    (
        932,
        "<<=",
        "b",
        false,
        false,
        869,
        869,
        16,
        934,
        0,
        "network_subeq",
        "networksel",
        "networkjoinsel",
    ),
    (
        933,
        ">>",
        "b",
        false,
        false,
        869,
        869,
        16,
        931,
        0,
        "network_sup",
        "networksel",
        "networkjoinsel",
    ),
    (
        934,
        ">>=",
        "b",
        false,
        false,
        869,
        869,
        16,
        932,
        0,
        "network_supeq",
        "networksel",
        "networkjoinsel",
    ),
    (
        965, "^", "b", false, false, 701, 701, 701, 0, 0, "dpow", "-", "-",
    ),
    (
        969,
        "@@",
        "l",
        false,
        false,
        0,
        601,
        600,
        0,
        0,
        "lseg_center",
        "-",
        "-",
    ),
    (
        974,
        "=",
        "b",
        false,
        true,
        1033,
        1033,
        16,
        974,
        0,
        "aclitemeq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        1038,
        "^",
        "b",
        false,
        false,
        1700,
        1700,
        1700,
        0,
        0,
        "numeric_power",
        "-",
        "-",
    ),
    (
        1054,
        "=",
        "b",
        true,
        true,
        1042,
        1042,
        16,
        1054,
        1057,
        "bpchareq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        1055,
        "~",
        "b",
        false,
        false,
        1042,
        25,
        16,
        0,
        1056,
        "bpcharregexeq",
        "regexeqsel",
        "regexeqjoinsel",
    ),
    (
        1056,
        "!~",
        "b",
        false,
        false,
        1042,
        25,
        16,
        0,
        1055,
        "bpcharregexne",
        "regexnesel",
        "regexnejoinsel",
    ),
    (
        1057,
        "<>",
        "b",
        false,
        false,
        1042,
        1042,
        16,
        1057,
        1054,
        "bpcharne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        1058,
        "<",
        "b",
        false,
        false,
        1042,
        1042,
        16,
        1060,
        1061,
        "bpcharlt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        1059,
        "<=",
        "b",
        false,
        false,
        1042,
        1042,
        16,
        1061,
        1060,
        "bpcharle",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        1060,
        ">",
        "b",
        false,
        false,
        1042,
        1042,
        16,
        1058,
        1059,
        "bpchargt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        1061,
        ">=",
        "b",
        false,
        false,
        1042,
        1042,
        16,
        1059,
        1058,
        "bpcharge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        1070,
        "=",
        "b",
        true,
        true,
        2277,
        2277,
        16,
        1070,
        1071,
        "array_eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        1071,
        "<>",
        "b",
        false,
        false,
        2277,
        2277,
        16,
        1071,
        1070,
        "array_ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        1072,
        "<",
        "b",
        false,
        false,
        2277,
        2277,
        16,
        1073,
        1075,
        "array_lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        1073,
        ">",
        "b",
        false,
        false,
        2277,
        2277,
        16,
        1072,
        1074,
        "array_gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        1074,
        "<=",
        "b",
        false,
        false,
        2277,
        2277,
        16,
        1075,
        1073,
        "array_le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        1075,
        ">=",
        "b",
        false,
        false,
        2277,
        2277,
        16,
        1074,
        1072,
        "array_ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        1076,
        "+",
        "b",
        false,
        false,
        1082,
        1186,
        1114,
        2551,
        0,
        "date_pl_interval",
        "-",
        "-",
    ),
    (
        1077,
        "-",
        "b",
        false,
        false,
        1082,
        1186,
        1114,
        0,
        0,
        "date_mi_interval",
        "-",
        "-",
    ),
    (
        1093,
        "=",
        "b",
        true,
        true,
        1082,
        1082,
        16,
        1093,
        1094,
        "date_eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        1094,
        "<>",
        "b",
        false,
        false,
        1082,
        1082,
        16,
        1094,
        1093,
        "date_ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        1095,
        "<",
        "b",
        false,
        false,
        1082,
        1082,
        16,
        1097,
        1098,
        "date_lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        1096,
        "<=",
        "b",
        false,
        false,
        1082,
        1082,
        16,
        1098,
        1097,
        "date_le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        1097,
        ">",
        "b",
        false,
        false,
        1082,
        1082,
        16,
        1095,
        1096,
        "date_gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        1098,
        ">=",
        "b",
        false,
        false,
        1082,
        1082,
        16,
        1096,
        1095,
        "date_ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        1099, "-", "b", false, false, 1082, 1082, 23, 0, 0, "date_mi", "-", "-",
    ),
    (
        1100, "+", "b", false, false, 1082, 23, 1082, 2555, 0, "date_pli", "-", "-",
    ),
    (
        1101, "-", "b", false, false, 1082, 23, 1082, 0, 0, "date_mii", "-", "-",
    ),
    (
        1108,
        "=",
        "b",
        true,
        true,
        1083,
        1083,
        16,
        1108,
        1109,
        "time_eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        1109,
        "<>",
        "b",
        false,
        false,
        1083,
        1083,
        16,
        1109,
        1108,
        "time_ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        1110,
        "<",
        "b",
        false,
        false,
        1083,
        1083,
        16,
        1112,
        1113,
        "time_lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        1111,
        "<=",
        "b",
        false,
        false,
        1083,
        1083,
        16,
        1113,
        1112,
        "time_le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        1112,
        ">",
        "b",
        false,
        false,
        1083,
        1083,
        16,
        1110,
        1111,
        "time_gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        1113,
        ">=",
        "b",
        false,
        false,
        1083,
        1083,
        16,
        1111,
        1110,
        "time_ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        1116,
        "+",
        "b",
        false,
        false,
        700,
        701,
        701,
        1126,
        0,
        "float48pl",
        "-",
        "-",
    ),
    (
        1117,
        "-",
        "b",
        false,
        false,
        700,
        701,
        701,
        0,
        0,
        "float48mi",
        "-",
        "-",
    ),
    (
        1118,
        "/",
        "b",
        false,
        false,
        700,
        701,
        701,
        0,
        0,
        "float48div",
        "-",
        "-",
    ),
    (
        1119,
        "*",
        "b",
        false,
        false,
        700,
        701,
        701,
        1129,
        0,
        "float48mul",
        "-",
        "-",
    ),
    (
        1120,
        "=",
        "b",
        true,
        true,
        700,
        701,
        16,
        1130,
        1121,
        "float48eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        1121,
        "<>",
        "b",
        false,
        false,
        700,
        701,
        16,
        1131,
        1120,
        "float48ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        1122,
        "<",
        "b",
        false,
        false,
        700,
        701,
        16,
        1133,
        1125,
        "float48lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        1123,
        ">",
        "b",
        false,
        false,
        700,
        701,
        16,
        1132,
        1124,
        "float48gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        1124,
        "<=",
        "b",
        false,
        false,
        700,
        701,
        16,
        1135,
        1123,
        "float48le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        1125,
        ">=",
        "b",
        false,
        false,
        700,
        701,
        16,
        1134,
        1122,
        "float48ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        1126,
        "+",
        "b",
        false,
        false,
        701,
        700,
        701,
        1116,
        0,
        "float84pl",
        "-",
        "-",
    ),
    (
        1127,
        "-",
        "b",
        false,
        false,
        701,
        700,
        701,
        0,
        0,
        "float84mi",
        "-",
        "-",
    ),
    (
        1128,
        "/",
        "b",
        false,
        false,
        701,
        700,
        701,
        0,
        0,
        "float84div",
        "-",
        "-",
    ),
    (
        1129,
        "*",
        "b",
        false,
        false,
        701,
        700,
        701,
        1119,
        0,
        "float84mul",
        "-",
        "-",
    ),
    (
        1130,
        "=",
        "b",
        true,
        true,
        701,
        700,
        16,
        1120,
        1131,
        "float84eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        1131,
        "<>",
        "b",
        false,
        false,
        701,
        700,
        16,
        1121,
        1130,
        "float84ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        1132,
        "<",
        "b",
        false,
        false,
        701,
        700,
        16,
        1123,
        1135,
        "float84lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        1133,
        ">",
        "b",
        false,
        false,
        701,
        700,
        16,
        1122,
        1134,
        "float84gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        1134,
        "<=",
        "b",
        false,
        false,
        701,
        700,
        16,
        1125,
        1133,
        "float84le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        1135,
        ">=",
        "b",
        false,
        false,
        701,
        700,
        16,
        1124,
        1132,
        "float84ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        1201,
        "=",
        "b",
        true,
        true,
        869,
        869,
        16,
        1201,
        1202,
        "network_eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        1202,
        "<>",
        "b",
        false,
        false,
        869,
        869,
        16,
        1202,
        1201,
        "network_ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        1203,
        "<",
        "b",
        false,
        false,
        869,
        869,
        16,
        1205,
        1206,
        "network_lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        1204,
        "<=",
        "b",
        false,
        false,
        869,
        869,
        16,
        1206,
        1205,
        "network_le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        1205,
        ">",
        "b",
        false,
        false,
        869,
        869,
        16,
        1203,
        1204,
        "network_gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        1206,
        ">=",
        "b",
        false,
        false,
        869,
        869,
        16,
        1204,
        1203,
        "network_ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        1207,
        "~~",
        "b",
        false,
        false,
        19,
        25,
        16,
        0,
        1208,
        "namelike",
        "likesel",
        "likejoinsel",
    ),
    (
        1208,
        "!~~",
        "b",
        false,
        false,
        19,
        25,
        16,
        0,
        1207,
        "namenlike",
        "nlikesel",
        "nlikejoinsel",
    ),
    (
        1209,
        "~~",
        "b",
        false,
        false,
        25,
        25,
        16,
        0,
        1210,
        "textlike",
        "likesel",
        "likejoinsel",
    ),
    (
        1210,
        "!~~",
        "b",
        false,
        false,
        25,
        25,
        16,
        0,
        1209,
        "textnlike",
        "nlikesel",
        "nlikejoinsel",
    ),
    (
        1211,
        "~~",
        "b",
        false,
        false,
        1042,
        25,
        16,
        0,
        1212,
        "bpcharlike",
        "likesel",
        "likejoinsel",
    ),
    (
        1212,
        "!~~",
        "b",
        false,
        false,
        1042,
        25,
        16,
        0,
        1211,
        "bpcharnlike",
        "nlikesel",
        "nlikejoinsel",
    ),
    (
        1220,
        "=",
        "b",
        true,
        true,
        829,
        829,
        16,
        1220,
        1221,
        "macaddr_eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        1221,
        "<>",
        "b",
        false,
        false,
        829,
        829,
        16,
        1221,
        1220,
        "macaddr_ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        1222,
        "<",
        "b",
        false,
        false,
        829,
        829,
        16,
        1224,
        1225,
        "macaddr_lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        1223,
        "<=",
        "b",
        false,
        false,
        829,
        829,
        16,
        1225,
        1224,
        "macaddr_le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        1224,
        ">",
        "b",
        false,
        false,
        829,
        829,
        16,
        1222,
        1223,
        "macaddr_gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        1225,
        ">=",
        "b",
        false,
        false,
        829,
        829,
        16,
        1223,
        1222,
        "macaddr_ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        1226,
        "~*",
        "b",
        false,
        false,
        19,
        25,
        16,
        0,
        1227,
        "nameicregexeq",
        "icregexeqsel",
        "icregexeqjoinsel",
    ),
    (
        1227,
        "!~*",
        "b",
        false,
        false,
        19,
        25,
        16,
        0,
        1226,
        "nameicregexne",
        "icregexnesel",
        "icregexnejoinsel",
    ),
    (
        1228,
        "~*",
        "b",
        false,
        false,
        25,
        25,
        16,
        0,
        1229,
        "texticregexeq",
        "icregexeqsel",
        "icregexeqjoinsel",
    ),
    (
        1229,
        "!~*",
        "b",
        false,
        false,
        25,
        25,
        16,
        0,
        1228,
        "texticregexne",
        "icregexnesel",
        "icregexnejoinsel",
    ),
    (
        1234,
        "~*",
        "b",
        false,
        false,
        1042,
        25,
        16,
        0,
        1235,
        "bpcharicregexeq",
        "icregexeqsel",
        "icregexeqjoinsel",
    ),
    (
        1235,
        "!~*",
        "b",
        false,
        false,
        1042,
        25,
        16,
        0,
        1234,
        "bpcharicregexne",
        "icregexnesel",
        "icregexnejoinsel",
    ),
    (
        1320,
        "=",
        "b",
        true,
        true,
        1184,
        1184,
        16,
        1320,
        1321,
        "timestamptz_eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        1321,
        "<>",
        "b",
        false,
        false,
        1184,
        1184,
        16,
        1321,
        1320,
        "timestamptz_ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        1322,
        "<",
        "b",
        false,
        false,
        1184,
        1184,
        16,
        1324,
        1325,
        "timestamptz_lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        1323,
        "<=",
        "b",
        false,
        false,
        1184,
        1184,
        16,
        1325,
        1324,
        "timestamptz_le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        1324,
        ">",
        "b",
        false,
        false,
        1184,
        1184,
        16,
        1322,
        1323,
        "timestamptz_gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        1325,
        ">=",
        "b",
        false,
        false,
        1184,
        1184,
        16,
        1323,
        1322,
        "timestamptz_ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        1327,
        "+",
        "b",
        false,
        false,
        1184,
        1186,
        1184,
        2554,
        0,
        "timestamptz_pl_interval",
        "-",
        "-",
    ),
    (
        1328,
        "-",
        "b",
        false,
        false,
        1184,
        1184,
        1186,
        0,
        0,
        "timestamptz_mi",
        "-",
        "-",
    ),
    (
        1329,
        "-",
        "b",
        false,
        false,
        1184,
        1186,
        1184,
        0,
        0,
        "timestamptz_mi_interval",
        "-",
        "-",
    ),
    (
        1330,
        "=",
        "b",
        true,
        true,
        1186,
        1186,
        16,
        1330,
        1331,
        "interval_eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        1331,
        "<>",
        "b",
        false,
        false,
        1186,
        1186,
        16,
        1331,
        1330,
        "interval_ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        1332,
        "<",
        "b",
        false,
        false,
        1186,
        1186,
        16,
        1334,
        1335,
        "interval_lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        1333,
        "<=",
        "b",
        false,
        false,
        1186,
        1186,
        16,
        1335,
        1334,
        "interval_le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        1334,
        ">",
        "b",
        false,
        false,
        1186,
        1186,
        16,
        1332,
        1333,
        "interval_gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        1335,
        ">=",
        "b",
        false,
        false,
        1186,
        1186,
        16,
        1333,
        1332,
        "interval_ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        1336,
        "-",
        "l",
        false,
        false,
        0,
        1186,
        1186,
        0,
        0,
        "interval_um",
        "-",
        "-",
    ),
    (
        1337,
        "+",
        "b",
        false,
        false,
        1186,
        1186,
        1186,
        1337,
        0,
        "interval_pl",
        "-",
        "-",
    ),
    (
        1338,
        "-",
        "b",
        false,
        false,
        1186,
        1186,
        1186,
        0,
        0,
        "interval_mi",
        "-",
        "-",
    ),
    (
        1360,
        "+",
        "b",
        false,
        false,
        1082,
        1083,
        1114,
        1363,
        0,
        "datetime_pl",
        "-",
        "-",
    ),
    (
        1363,
        "+",
        "b",
        false,
        false,
        1083,
        1082,
        1114,
        1360,
        0,
        "timedate_pl",
        "-",
        "-",
    ),
    (
        1399,
        "-",
        "b",
        false,
        false,
        1083,
        1083,
        1186,
        0,
        0,
        "time_mi_time",
        "-",
        "-",
    ),
    (
        1420,
        "@@",
        "l",
        false,
        false,
        0,
        718,
        600,
        0,
        0,
        "circle_center",
        "-",
        "-",
    ),
    (
        1500,
        "=",
        "b",
        false,
        false,
        718,
        718,
        16,
        1500,
        1501,
        "circle_eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        1501,
        "<>",
        "b",
        false,
        false,
        718,
        718,
        16,
        1501,
        1500,
        "circle_ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        1502,
        "<",
        "b",
        false,
        false,
        718,
        718,
        16,
        1503,
        1505,
        "circle_lt",
        "areasel",
        "areajoinsel",
    ),
    (
        1503,
        ">",
        "b",
        false,
        false,
        718,
        718,
        16,
        1502,
        1504,
        "circle_gt",
        "areasel",
        "areajoinsel",
    ),
    (
        1504,
        "<=",
        "b",
        false,
        false,
        718,
        718,
        16,
        1505,
        1503,
        "circle_le",
        "areasel",
        "areajoinsel",
    ),
    (
        1505,
        ">=",
        "b",
        false,
        false,
        718,
        718,
        16,
        1504,
        1502,
        "circle_ge",
        "areasel",
        "areajoinsel",
    ),
    (
        1510,
        "<@",
        "b",
        false,
        false,
        718,
        718,
        16,
        1511,
        0,
        "circle_contained",
        "contsel",
        "contjoinsel",
    ),
    (
        1511,
        "@>",
        "b",
        false,
        false,
        718,
        718,
        16,
        1510,
        0,
        "circle_contain",
        "contsel",
        "contjoinsel",
    ),
    (
        1512,
        "~=",
        "b",
        false,
        false,
        718,
        718,
        16,
        1512,
        0,
        "circle_same",
        "eqsel",
        "eqjoinsel",
    ),
    (
        1513,
        "&&",
        "b",
        false,
        false,
        718,
        718,
        16,
        1513,
        0,
        "circle_overlap",
        "areasel",
        "areajoinsel",
    ),
    (
        1518,
        "*",
        "b",
        false,
        false,
        718,
        600,
        718,
        0,
        0,
        "circle_mul_pt",
        "-",
        "-",
    ),
    (
        1519,
        "/",
        "b",
        false,
        false,
        718,
        600,
        718,
        0,
        0,
        "circle_div_pt",
        "-",
        "-",
    ),
    (
        1520,
        "<->",
        "b",
        false,
        false,
        718,
        718,
        701,
        1520,
        0,
        "circle_distance",
        "-",
        "-",
    ),
    (
        1521,
        "#",
        "l",
        false,
        false,
        0,
        604,
        23,
        0,
        0,
        "poly_npoints",
        "-",
        "-",
    ),
    (
        1522, "<->", "b", false, false, 600, 718, 701, 3291, 0, "dist_pc", "-", "-",
    ),
    (
        1525,
        "?#",
        "b",
        false,
        false,
        601,
        601,
        16,
        1525,
        0,
        "lseg_intersect",
        "-",
        "-",
    ),
    (
        1526,
        "?||",
        "b",
        false,
        false,
        601,
        601,
        16,
        1526,
        0,
        "lseg_parallel",
        "-",
        "-",
    ),
    (
        1527,
        "?-|",
        "b",
        false,
        false,
        601,
        601,
        16,
        1527,
        0,
        "lseg_perp",
        "-",
        "-",
    ),
    (
        1528,
        "?-",
        "l",
        false,
        false,
        0,
        601,
        16,
        0,
        0,
        "lseg_horizontal",
        "-",
        "-",
    ),
    (
        1529,
        "?|",
        "l",
        false,
        false,
        0,
        601,
        16,
        0,
        0,
        "lseg_vertical",
        "-",
        "-",
    ),
    (
        1536,
        "#",
        "b",
        false,
        false,
        601,
        601,
        600,
        1536,
        0,
        "lseg_interpt",
        "-",
        "-",
    ),
    (
        1537, "?#", "b", false, false, 601, 628, 16, 0, 0, "inter_sl", "-", "-",
    ),
    (
        1538, "?#", "b", false, false, 601, 603, 16, 0, 0, "inter_sb", "-", "-",
    ),
    (
        1539, "?#", "b", false, false, 628, 603, 16, 0, 0, "inter_lb", "-", "-",
    ),
    (
        1547, "<@", "b", false, false, 600, 601, 16, 0, 0, "on_ps", "-", "-",
    ),
    (
        1550,
        "=",
        "b",
        true,
        true,
        1266,
        1266,
        16,
        1550,
        1551,
        "timetz_eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        1551,
        "<>",
        "b",
        false,
        false,
        1266,
        1266,
        16,
        1551,
        1550,
        "timetz_ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        1552,
        "<",
        "b",
        false,
        false,
        1266,
        1266,
        16,
        1554,
        1555,
        "timetz_lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        1553,
        "<=",
        "b",
        false,
        false,
        1266,
        1266,
        16,
        1555,
        1554,
        "timetz_le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        1554,
        ">",
        "b",
        false,
        false,
        1266,
        1266,
        16,
        1552,
        1553,
        "timetz_gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        1555,
        ">=",
        "b",
        false,
        false,
        1266,
        1266,
        16,
        1553,
        1552,
        "timetz_ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        1558, "##", "b", false, false, 600, 601, 600, 0, 0, "close_ps", "-", "-",
    ),
    (
        1559, "##", "b", false, false, 600, 603, 600, 0, 0, "close_pb", "-", "-",
    ),
    (
        1583,
        "*",
        "b",
        false,
        false,
        1186,
        701,
        1186,
        1584,
        0,
        "interval_mul",
        "-",
        "-",
    ),
    (
        1584,
        "*",
        "b",
        false,
        false,
        701,
        1186,
        1186,
        1583,
        0,
        "mul_d_interval",
        "-",
        "-",
    ),
    (
        1585,
        "/",
        "b",
        false,
        false,
        1186,
        701,
        1186,
        0,
        0,
        "interval_div",
        "-",
        "-",
    ),
    (
        1591,
        "@-@",
        "l",
        false,
        false,
        0,
        601,
        701,
        0,
        0,
        "lseg_length",
        "-",
        "-",
    ),
    (
        1611,
        "?#",
        "b",
        false,
        false,
        628,
        628,
        16,
        1611,
        0,
        "line_intersect",
        "-",
        "-",
    ),
    (
        1612,
        "?||",
        "b",
        false,
        false,
        628,
        628,
        16,
        1612,
        0,
        "line_parallel",
        "-",
        "-",
    ),
    (
        1613,
        "?-|",
        "b",
        false,
        false,
        628,
        628,
        16,
        1613,
        0,
        "line_perp",
        "-",
        "-",
    ),
    (
        1617,
        "#",
        "b",
        false,
        false,
        628,
        628,
        600,
        1617,
        0,
        "line_interpt",
        "-",
        "-",
    ),
    (
        1625,
        "~~*",
        "b",
        false,
        false,
        19,
        25,
        16,
        0,
        1626,
        "nameiclike",
        "iclikesel",
        "iclikejoinsel",
    ),
    (
        1626,
        "!~~*",
        "b",
        false,
        false,
        19,
        25,
        16,
        0,
        1625,
        "nameicnlike",
        "icnlikesel",
        "icnlikejoinsel",
    ),
    (
        1627,
        "~~*",
        "b",
        false,
        false,
        25,
        25,
        16,
        0,
        1628,
        "texticlike",
        "iclikesel",
        "iclikejoinsel",
    ),
    (
        1628,
        "!~~*",
        "b",
        false,
        false,
        25,
        25,
        16,
        0,
        1627,
        "texticnlike",
        "icnlikesel",
        "icnlikejoinsel",
    ),
    (
        1629,
        "~~*",
        "b",
        false,
        false,
        1042,
        25,
        16,
        0,
        1630,
        "bpchariclike",
        "iclikesel",
        "iclikejoinsel",
    ),
    (
        1630,
        "!~~*",
        "b",
        false,
        false,
        1042,
        25,
        16,
        0,
        1629,
        "bpcharicnlike",
        "icnlikesel",
        "icnlikejoinsel",
    ),
    (
        1694,
        "<=",
        "b",
        false,
        false,
        16,
        16,
        16,
        1695,
        59,
        "boolle",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        1695,
        ">=",
        "b",
        false,
        false,
        16,
        16,
        16,
        1694,
        58,
        "boolge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        1751,
        "-",
        "l",
        false,
        false,
        0,
        1700,
        1700,
        0,
        0,
        "numeric_uminus",
        "-",
        "-",
    ),
    (
        1752,
        "=",
        "b",
        true,
        true,
        1700,
        1700,
        16,
        1752,
        1753,
        "numeric_eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        1753,
        "<>",
        "b",
        false,
        false,
        1700,
        1700,
        16,
        1753,
        1752,
        "numeric_ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        1754,
        "<",
        "b",
        false,
        false,
        1700,
        1700,
        16,
        1756,
        1757,
        "numeric_lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        1755,
        "<=",
        "b",
        false,
        false,
        1700,
        1700,
        16,
        1757,
        1756,
        "numeric_le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        1756,
        ">",
        "b",
        false,
        false,
        1700,
        1700,
        16,
        1754,
        1755,
        "numeric_gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        1757,
        ">=",
        "b",
        false,
        false,
        1700,
        1700,
        16,
        1755,
        1754,
        "numeric_ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        1758,
        "+",
        "b",
        false,
        false,
        1700,
        1700,
        1700,
        1758,
        0,
        "numeric_add",
        "-",
        "-",
    ),
    (
        1759,
        "-",
        "b",
        false,
        false,
        1700,
        1700,
        1700,
        0,
        0,
        "numeric_sub",
        "-",
        "-",
    ),
    (
        1760,
        "*",
        "b",
        false,
        false,
        1700,
        1700,
        1700,
        1760,
        0,
        "numeric_mul",
        "-",
        "-",
    ),
    (
        1761,
        "/",
        "b",
        false,
        false,
        1700,
        1700,
        1700,
        0,
        0,
        "numeric_div",
        "-",
        "-",
    ),
    (
        1762,
        "%",
        "b",
        false,
        false,
        1700,
        1700,
        1700,
        0,
        0,
        "numeric_mod",
        "-",
        "-",
    ),
    (
        1763,
        "@",
        "l",
        false,
        false,
        0,
        1700,
        1700,
        0,
        0,
        "numeric_abs",
        "-",
        "-",
    ),
    (
        1784,
        "=",
        "b",
        true,
        false,
        1560,
        1560,
        16,
        1784,
        1785,
        "biteq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        1785,
        "<>",
        "b",
        false,
        false,
        1560,
        1560,
        16,
        1785,
        1784,
        "bitne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        1786,
        "<",
        "b",
        false,
        false,
        1560,
        1560,
        16,
        1787,
        1789,
        "bitlt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        1787,
        ">",
        "b",
        false,
        false,
        1560,
        1560,
        16,
        1786,
        1788,
        "bitgt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        1788,
        "<=",
        "b",
        false,
        false,
        1560,
        1560,
        16,
        1789,
        1787,
        "bitle",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        1789,
        ">=",
        "b",
        false,
        false,
        1560,
        1560,
        16,
        1788,
        1786,
        "bitge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        1791, "&", "b", false, false, 1560, 1560, 1560, 1791, 0, "bitand", "-", "-",
    ),
    (
        1792, "|", "b", false, false, 1560, 1560, 1560, 1792, 0, "bitor", "-", "-",
    ),
    (
        1793, "#", "b", false, false, 1560, 1560, 1560, 1793, 0, "bitxor", "-", "-",
    ),
    (
        1794, "~", "l", false, false, 0, 1560, 1560, 0, 0, "bitnot", "-", "-",
    ),
    (
        1795,
        "<<",
        "b",
        false,
        false,
        1560,
        23,
        1560,
        0,
        0,
        "bitshiftleft",
        "-",
        "-",
    ),
    (
        1796,
        ">>",
        "b",
        false,
        false,
        1560,
        23,
        1560,
        0,
        0,
        "bitshiftright",
        "-",
        "-",
    ),
    (
        1797, "||", "b", false, false, 1562, 1562, 1562, 0, 0, "bitcat", "-", "-",
    ),
    (
        1800,
        "+",
        "b",
        false,
        false,
        1083,
        1186,
        1083,
        1849,
        0,
        "time_pl_interval",
        "-",
        "-",
    ),
    (
        1801,
        "-",
        "b",
        false,
        false,
        1083,
        1186,
        1083,
        0,
        0,
        "time_mi_interval",
        "-",
        "-",
    ),
    (
        1804,
        "=",
        "b",
        true,
        false,
        1562,
        1562,
        16,
        1804,
        1805,
        "varbiteq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        1805,
        "<>",
        "b",
        false,
        false,
        1562,
        1562,
        16,
        1805,
        1804,
        "varbitne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        1806,
        "<",
        "b",
        false,
        false,
        1562,
        1562,
        16,
        1807,
        1809,
        "varbitlt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        1807,
        ">",
        "b",
        false,
        false,
        1562,
        1562,
        16,
        1806,
        1808,
        "varbitgt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        1808,
        "<=",
        "b",
        false,
        false,
        1562,
        1562,
        16,
        1809,
        1807,
        "varbitle",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        1809,
        ">=",
        "b",
        false,
        false,
        1562,
        1562,
        16,
        1808,
        1806,
        "varbitge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        1849,
        "+",
        "b",
        false,
        false,
        1186,
        1083,
        1083,
        1800,
        0,
        "interval_pl_time",
        "-",
        "-",
    ),
    (
        1862,
        "=",
        "b",
        true,
        true,
        21,
        20,
        16,
        1868,
        1863,
        "int28eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        1863,
        "<>",
        "b",
        false,
        false,
        21,
        20,
        16,
        1869,
        1862,
        "int28ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        1864,
        "<",
        "b",
        false,
        false,
        21,
        20,
        16,
        1871,
        1867,
        "int28lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        1865,
        ">",
        "b",
        false,
        false,
        21,
        20,
        16,
        1870,
        1866,
        "int28gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        1866,
        "<=",
        "b",
        false,
        false,
        21,
        20,
        16,
        1873,
        1865,
        "int28le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        1867,
        ">=",
        "b",
        false,
        false,
        21,
        20,
        16,
        1872,
        1864,
        "int28ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        1868,
        "=",
        "b",
        true,
        true,
        20,
        21,
        16,
        1862,
        1869,
        "int82eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        1869,
        "<>",
        "b",
        false,
        false,
        20,
        21,
        16,
        1863,
        1868,
        "int82ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        1870,
        "<",
        "b",
        false,
        false,
        20,
        21,
        16,
        1865,
        1873,
        "int82lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        1871,
        ">",
        "b",
        false,
        false,
        20,
        21,
        16,
        1864,
        1872,
        "int82gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        1872,
        "<=",
        "b",
        false,
        false,
        20,
        21,
        16,
        1867,
        1871,
        "int82le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        1873,
        ">=",
        "b",
        false,
        false,
        20,
        21,
        16,
        1866,
        1870,
        "int82ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        1874, "&", "b", false, false, 21, 21, 21, 1874, 0, "int2and", "-", "-",
    ),
    (
        1875, "|", "b", false, false, 21, 21, 21, 1875, 0, "int2or", "-", "-",
    ),
    (
        1876, "#", "b", false, false, 21, 21, 21, 1876, 0, "int2xor", "-", "-",
    ),
    (
        1877, "~", "l", false, false, 0, 21, 21, 0, 0, "int2not", "-", "-",
    ),
    (
        1878, "<<", "b", false, false, 21, 23, 21, 0, 0, "int2shl", "-", "-",
    ),
    (
        1879, ">>", "b", false, false, 21, 23, 21, 0, 0, "int2shr", "-", "-",
    ),
    (
        1880, "&", "b", false, false, 23, 23, 23, 1880, 0, "int4and", "-", "-",
    ),
    (
        1881, "|", "b", false, false, 23, 23, 23, 1881, 0, "int4or", "-", "-",
    ),
    (
        1882, "#", "b", false, false, 23, 23, 23, 1882, 0, "int4xor", "-", "-",
    ),
    (
        1883, "~", "l", false, false, 0, 23, 23, 0, 0, "int4not", "-", "-",
    ),
    (
        1884, "<<", "b", false, false, 23, 23, 23, 0, 0, "int4shl", "-", "-",
    ),
    (
        1885, ">>", "b", false, false, 23, 23, 23, 0, 0, "int4shr", "-", "-",
    ),
    (
        1886, "&", "b", false, false, 20, 20, 20, 1886, 0, "int8and", "-", "-",
    ),
    (
        1887, "|", "b", false, false, 20, 20, 20, 1887, 0, "int8or", "-", "-",
    ),
    (
        1888, "#", "b", false, false, 20, 20, 20, 1888, 0, "int8xor", "-", "-",
    ),
    (
        1889, "~", "l", false, false, 0, 20, 20, 0, 0, "int8not", "-", "-",
    ),
    (
        1890, "<<", "b", false, false, 20, 23, 20, 0, 0, "int8shl", "-", "-",
    ),
    (
        1891, ">>", "b", false, false, 20, 23, 20, 0, 0, "int8shr", "-", "-",
    ),
    (
        1916, "+", "l", false, false, 0, 20, 20, 0, 0, "int8up", "-", "-",
    ),
    (
        1917, "+", "l", false, false, 0, 21, 21, 0, 0, "int2up", "-", "-",
    ),
    (
        1918, "+", "l", false, false, 0, 23, 23, 0, 0, "int4up", "-", "-",
    ),
    (
        1919, "+", "l", false, false, 0, 700, 700, 0, 0, "float4up", "-", "-",
    ),
    (
        1920, "+", "l", false, false, 0, 701, 701, 0, 0, "float8up", "-", "-",
    ),
    (
        1921,
        "+",
        "l",
        false,
        false,
        0,
        1700,
        1700,
        0,
        0,
        "numeric_uplus",
        "-",
        "-",
    ),
    (
        1955,
        "=",
        "b",
        true,
        true,
        17,
        17,
        16,
        1955,
        1956,
        "byteaeq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        1956,
        "<>",
        "b",
        false,
        false,
        17,
        17,
        16,
        1956,
        1955,
        "byteane",
        "neqsel",
        "neqjoinsel",
    ),
    (
        1957,
        "<",
        "b",
        false,
        false,
        17,
        17,
        16,
        1959,
        1960,
        "bytealt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        1958,
        "<=",
        "b",
        false,
        false,
        17,
        17,
        16,
        1960,
        1959,
        "byteale",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        1959,
        ">",
        "b",
        false,
        false,
        17,
        17,
        16,
        1957,
        1958,
        "byteagt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        1960,
        ">=",
        "b",
        false,
        false,
        17,
        17,
        16,
        1958,
        1957,
        "byteage",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        2018, "||", "b", false, false, 17, 17, 17, 0, 0, "byteacat", "-", "-",
    ),
    (
        2060,
        "=",
        "b",
        true,
        true,
        1114,
        1114,
        16,
        2060,
        2061,
        "timestamp_eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        2061,
        "<>",
        "b",
        false,
        false,
        1114,
        1114,
        16,
        2061,
        2060,
        "timestamp_ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        2062,
        "<",
        "b",
        false,
        false,
        1114,
        1114,
        16,
        2064,
        2065,
        "timestamp_lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        2063,
        "<=",
        "b",
        false,
        false,
        1114,
        1114,
        16,
        2065,
        2064,
        "timestamp_le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        2064,
        ">",
        "b",
        false,
        false,
        1114,
        1114,
        16,
        2062,
        2063,
        "timestamp_gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        2065,
        ">=",
        "b",
        false,
        false,
        1114,
        1114,
        16,
        2063,
        2062,
        "timestamp_ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        2066,
        "+",
        "b",
        false,
        false,
        1114,
        1186,
        1114,
        2553,
        0,
        "timestamp_pl_interval",
        "-",
        "-",
    ),
    (
        2067,
        "-",
        "b",
        false,
        false,
        1114,
        1114,
        1186,
        0,
        0,
        "timestamp_mi",
        "-",
        "-",
    ),
    (
        2068,
        "-",
        "b",
        false,
        false,
        1114,
        1186,
        1114,
        0,
        0,
        "timestamp_mi_interval",
        "-",
        "-",
    ),
    (
        2314,
        "~<~",
        "b",
        false,
        false,
        25,
        25,
        16,
        2318,
        2317,
        "text_pattern_lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        2315,
        "~<=~",
        "b",
        false,
        false,
        25,
        25,
        16,
        2317,
        2318,
        "text_pattern_le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        2317,
        "~>=~",
        "b",
        false,
        false,
        25,
        25,
        16,
        2315,
        2314,
        "text_pattern_ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        2318,
        "~>~",
        "b",
        false,
        false,
        25,
        25,
        16,
        2314,
        2315,
        "text_pattern_gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        2345,
        "<",
        "b",
        false,
        false,
        1082,
        1114,
        16,
        2375,
        2348,
        "date_lt_timestamp",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        2346,
        "<=",
        "b",
        false,
        false,
        1082,
        1114,
        16,
        2374,
        2349,
        "date_le_timestamp",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        2347,
        "=",
        "b",
        true,
        false,
        1082,
        1114,
        16,
        2373,
        2350,
        "date_eq_timestamp",
        "eqsel",
        "eqjoinsel",
    ),
    (
        2348,
        ">=",
        "b",
        false,
        false,
        1082,
        1114,
        16,
        2372,
        2345,
        "date_ge_timestamp",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        2349,
        ">",
        "b",
        false,
        false,
        1082,
        1114,
        16,
        2371,
        2346,
        "date_gt_timestamp",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        2350,
        "<>",
        "b",
        false,
        false,
        1082,
        1114,
        16,
        2376,
        2347,
        "date_ne_timestamp",
        "neqsel",
        "neqjoinsel",
    ),
    (
        2358,
        "<",
        "b",
        false,
        false,
        1082,
        1184,
        16,
        2388,
        2361,
        "date_lt_timestamptz",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        2359,
        "<=",
        "b",
        false,
        false,
        1082,
        1184,
        16,
        2387,
        2362,
        "date_le_timestamptz",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        2360,
        "=",
        "b",
        true,
        false,
        1082,
        1184,
        16,
        2386,
        2363,
        "date_eq_timestamptz",
        "eqsel",
        "eqjoinsel",
    ),
    (
        2361,
        ">=",
        "b",
        false,
        false,
        1082,
        1184,
        16,
        2385,
        2358,
        "date_ge_timestamptz",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        2362,
        ">",
        "b",
        false,
        false,
        1082,
        1184,
        16,
        2384,
        2359,
        "date_gt_timestamptz",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        2363,
        "<>",
        "b",
        false,
        false,
        1082,
        1184,
        16,
        2389,
        2360,
        "date_ne_timestamptz",
        "neqsel",
        "neqjoinsel",
    ),
    (
        2371,
        "<",
        "b",
        false,
        false,
        1114,
        1082,
        16,
        2349,
        2374,
        "timestamp_lt_date",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        2372,
        "<=",
        "b",
        false,
        false,
        1114,
        1082,
        16,
        2348,
        2375,
        "timestamp_le_date",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        2373,
        "=",
        "b",
        true,
        false,
        1114,
        1082,
        16,
        2347,
        2376,
        "timestamp_eq_date",
        "eqsel",
        "eqjoinsel",
    ),
    (
        2374,
        ">=",
        "b",
        false,
        false,
        1114,
        1082,
        16,
        2346,
        2371,
        "timestamp_ge_date",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        2375,
        ">",
        "b",
        false,
        false,
        1114,
        1082,
        16,
        2345,
        2372,
        "timestamp_gt_date",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        2376,
        "<>",
        "b",
        false,
        false,
        1114,
        1082,
        16,
        2350,
        2373,
        "timestamp_ne_date",
        "neqsel",
        "neqjoinsel",
    ),
    (
        2384,
        "<",
        "b",
        false,
        false,
        1184,
        1082,
        16,
        2362,
        2387,
        "timestamptz_lt_date",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        2385,
        "<=",
        "b",
        false,
        false,
        1184,
        1082,
        16,
        2361,
        2388,
        "timestamptz_le_date",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        2386,
        "=",
        "b",
        true,
        false,
        1184,
        1082,
        16,
        2360,
        2389,
        "timestamptz_eq_date",
        "eqsel",
        "eqjoinsel",
    ),
    (
        2387,
        ">=",
        "b",
        false,
        false,
        1184,
        1082,
        16,
        2359,
        2384,
        "timestamptz_ge_date",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        2388,
        ">",
        "b",
        false,
        false,
        1184,
        1082,
        16,
        2358,
        2385,
        "timestamptz_gt_date",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        2389,
        "<>",
        "b",
        false,
        false,
        1184,
        1082,
        16,
        2363,
        2386,
        "timestamptz_ne_date",
        "neqsel",
        "neqjoinsel",
    ),
    (
        2534,
        "<",
        "b",
        false,
        false,
        1114,
        1184,
        16,
        2544,
        2537,
        "timestamp_lt_timestamptz",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        2535,
        "<=",
        "b",
        false,
        false,
        1114,
        1184,
        16,
        2543,
        2538,
        "timestamp_le_timestamptz",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        2536,
        "=",
        "b",
        true,
        false,
        1114,
        1184,
        16,
        2542,
        2539,
        "timestamp_eq_timestamptz",
        "eqsel",
        "eqjoinsel",
    ),
    (
        2537,
        ">=",
        "b",
        false,
        false,
        1114,
        1184,
        16,
        2541,
        2534,
        "timestamp_ge_timestamptz",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        2538,
        ">",
        "b",
        false,
        false,
        1114,
        1184,
        16,
        2540,
        2535,
        "timestamp_gt_timestamptz",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        2539,
        "<>",
        "b",
        false,
        false,
        1114,
        1184,
        16,
        2545,
        2536,
        "timestamp_ne_timestamptz",
        "neqsel",
        "neqjoinsel",
    ),
    (
        2540,
        "<",
        "b",
        false,
        false,
        1184,
        1114,
        16,
        2538,
        2543,
        "timestamptz_lt_timestamp",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        2541,
        "<=",
        "b",
        false,
        false,
        1184,
        1114,
        16,
        2537,
        2544,
        "timestamptz_le_timestamp",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        2542,
        "=",
        "b",
        true,
        false,
        1184,
        1114,
        16,
        2536,
        2545,
        "timestamptz_eq_timestamp",
        "eqsel",
        "eqjoinsel",
    ),
    (
        2543,
        ">=",
        "b",
        false,
        false,
        1184,
        1114,
        16,
        2535,
        2540,
        "timestamptz_ge_timestamp",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        2544,
        ">",
        "b",
        false,
        false,
        1184,
        1114,
        16,
        2534,
        2541,
        "timestamptz_gt_timestamp",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        2545,
        "<>",
        "b",
        false,
        false,
        1184,
        1114,
        16,
        2539,
        2542,
        "timestamptz_ne_timestamp",
        "neqsel",
        "neqjoinsel",
    ),
    (
        2551,
        "+",
        "b",
        false,
        false,
        1186,
        1082,
        1114,
        1076,
        0,
        "interval_pl_date",
        "-",
        "-",
    ),
    (
        2553,
        "+",
        "b",
        false,
        false,
        1186,
        1114,
        1114,
        2066,
        0,
        "interval_pl_timestamp",
        "-",
        "-",
    ),
    (
        2554,
        "+",
        "b",
        false,
        false,
        1186,
        1184,
        1184,
        1327,
        0,
        "interval_pl_timestamptz",
        "-",
        "-",
    ),
    (
        2555,
        "+",
        "b",
        false,
        false,
        23,
        1082,
        1082,
        1100,
        0,
        "integer_pl_date",
        "-",
        "-",
    ),
    (
        2634, "~", "l", false, false, 0, 869, 869, 0, 0, "inetnot", "-", "-",
    ),
    (
        2635, "&", "b", false, false, 869, 869, 869, 0, 0, "inetand", "-", "-",
    ),
    (
        2636, "|", "b", false, false, 869, 869, 869, 0, 0, "inetor", "-", "-",
    ),
    (
        2637, "+", "b", false, false, 869, 20, 869, 2638, 0, "inetpl", "-", "-",
    ),
    (
        2638,
        "+",
        "b",
        false,
        false,
        20,
        869,
        869,
        2637,
        0,
        "int8pl_inet",
        "-",
        "-",
    ),
    (
        2639,
        "-",
        "b",
        false,
        false,
        869,
        20,
        869,
        0,
        0,
        "inetmi_int8",
        "-",
        "-",
    ),
    (
        2640, "-", "b", false, false, 869, 869, 20, 0, 0, "inetmi", "-", "-",
    ),
    (
        2750,
        "&&",
        "b",
        false,
        false,
        2277,
        2277,
        16,
        2750,
        0,
        "arrayoverlap",
        "arraycontsel",
        "arraycontjoinsel",
    ),
    (
        2751,
        "@>",
        "b",
        false,
        false,
        2277,
        2277,
        16,
        2752,
        0,
        "arraycontains",
        "arraycontsel",
        "arraycontjoinsel",
    ),
    (
        2752,
        "<@",
        "b",
        false,
        false,
        2277,
        2277,
        16,
        2751,
        0,
        "arraycontained",
        "arraycontsel",
        "arraycontjoinsel",
    ),
    (
        2779,
        "||",
        "b",
        false,
        false,
        25,
        2776,
        25,
        0,
        0,
        "textanycat",
        "-",
        "-",
    ),
    (
        2780,
        "||",
        "b",
        false,
        false,
        2776,
        25,
        25,
        0,
        0,
        "anytextcat",
        "-",
        "-",
    ),
    (
        2799,
        "<",
        "b",
        false,
        false,
        27,
        27,
        16,
        2800,
        2802,
        "tidlt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        2800,
        ">",
        "b",
        false,
        false,
        27,
        27,
        16,
        2799,
        2801,
        "tidgt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        2801,
        "<=",
        "b",
        false,
        false,
        27,
        27,
        16,
        2802,
        2800,
        "tidle",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        2802,
        ">=",
        "b",
        false,
        false,
        27,
        27,
        16,
        2801,
        2799,
        "tidge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        2860,
        "=",
        "b",
        true,
        true,
        4537,
        4537,
        16,
        2860,
        2861,
        "multirange_eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        2861,
        "<>",
        "b",
        false,
        false,
        4537,
        4537,
        16,
        2861,
        2860,
        "multirange_ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        2862,
        "<",
        "b",
        false,
        false,
        4537,
        4537,
        16,
        2865,
        2864,
        "multirange_lt",
        "multirangesel",
        "scalarltjoinsel",
    ),
    (
        2863,
        "<=",
        "b",
        false,
        false,
        4537,
        4537,
        16,
        2864,
        2865,
        "multirange_le",
        "multirangesel",
        "scalarlejoinsel",
    ),
    (
        2864,
        ">=",
        "b",
        false,
        false,
        4537,
        4537,
        16,
        2863,
        2862,
        "multirange_ge",
        "multirangesel",
        "scalargejoinsel",
    ),
    (
        2865,
        ">",
        "b",
        false,
        false,
        4537,
        4537,
        16,
        2862,
        2863,
        "multirange_gt",
        "multirangesel",
        "scalargtjoinsel",
    ),
    (
        2866,
        "&&",
        "b",
        false,
        false,
        3831,
        4537,
        16,
        2867,
        0,
        "range_overlaps_multirange",
        "multirangesel",
        "areajoinsel",
    ),
    (
        2867,
        "&&",
        "b",
        false,
        false,
        4537,
        3831,
        16,
        2866,
        0,
        "multirange_overlaps_range",
        "multirangesel",
        "areajoinsel",
    ),
    (
        2868,
        "&&",
        "b",
        false,
        false,
        4537,
        4537,
        16,
        2868,
        0,
        "multirange_overlaps_multirange",
        "multirangesel",
        "areajoinsel",
    ),
    (
        2869,
        "@>",
        "b",
        false,
        false,
        4537,
        2283,
        16,
        2872,
        0,
        "multirange_contains_elem",
        "multirangesel",
        "contjoinsel",
    ),
    (
        2870,
        "@>",
        "b",
        false,
        false,
        4537,
        3831,
        16,
        2873,
        0,
        "multirange_contains_range",
        "multirangesel",
        "contjoinsel",
    ),
    (
        2871,
        "@>",
        "b",
        false,
        false,
        4537,
        4537,
        16,
        2874,
        0,
        "multirange_contains_multirange",
        "multirangesel",
        "contjoinsel",
    ),
    (
        2872,
        "<@",
        "b",
        false,
        false,
        2283,
        4537,
        16,
        2869,
        0,
        "elem_contained_by_multirange",
        "multirangesel",
        "contjoinsel",
    ),
    (
        2873,
        "<@",
        "b",
        false,
        false,
        3831,
        4537,
        16,
        2870,
        0,
        "range_contained_by_multirange",
        "multirangesel",
        "contjoinsel",
    ),
    (
        2874,
        "<@",
        "b",
        false,
        false,
        4537,
        4537,
        16,
        2871,
        0,
        "multirange_contained_by_multirange",
        "multirangesel",
        "contjoinsel",
    ),
    (
        2875,
        "&<",
        "b",
        false,
        false,
        3831,
        4537,
        16,
        0,
        0,
        "range_overleft_multirange",
        "multirangesel",
        "scalarltjoinsel",
    ),
    (
        2876,
        "&<",
        "b",
        false,
        false,
        4537,
        3831,
        16,
        0,
        0,
        "multirange_overleft_range",
        "multirangesel",
        "scalarltjoinsel",
    ),
    (
        2877,
        "&<",
        "b",
        false,
        false,
        4537,
        4537,
        16,
        0,
        0,
        "multirange_overleft_multirange",
        "multirangesel",
        "scalarltjoinsel",
    ),
    (
        2972,
        "=",
        "b",
        true,
        true,
        2950,
        2950,
        16,
        2972,
        2973,
        "uuid_eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        2973,
        "<>",
        "b",
        false,
        false,
        2950,
        2950,
        16,
        2973,
        2972,
        "uuid_ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        2974,
        "<",
        "b",
        false,
        false,
        2950,
        2950,
        16,
        2975,
        2977,
        "uuid_lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        2975,
        ">",
        "b",
        false,
        false,
        2950,
        2950,
        16,
        2974,
        2976,
        "uuid_gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        2976,
        "<=",
        "b",
        false,
        false,
        2950,
        2950,
        16,
        2977,
        2975,
        "uuid_le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        2977,
        ">=",
        "b",
        false,
        false,
        2950,
        2950,
        16,
        2976,
        2974,
        "uuid_ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        2988,
        "=",
        "b",
        true,
        true,
        2249,
        2249,
        16,
        2988,
        2989,
        "record_eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        2989,
        "<>",
        "b",
        false,
        false,
        2249,
        2249,
        16,
        2989,
        2988,
        "record_ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        2990,
        "<",
        "b",
        false,
        false,
        2249,
        2249,
        16,
        2991,
        2993,
        "record_lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        2991,
        ">",
        "b",
        false,
        false,
        2249,
        2249,
        16,
        2990,
        2992,
        "record_gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        2992,
        "<=",
        "b",
        false,
        false,
        2249,
        2249,
        16,
        2993,
        2991,
        "record_le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        2993,
        ">=",
        "b",
        false,
        false,
        2249,
        2249,
        16,
        2992,
        2990,
        "record_ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        3147,
        "~",
        "l",
        false,
        false,
        0,
        829,
        829,
        0,
        0,
        "macaddr_not",
        "-",
        "-",
    ),
    (
        3148,
        "&",
        "b",
        false,
        false,
        829,
        829,
        829,
        0,
        0,
        "macaddr_and",
        "-",
        "-",
    ),
    (
        3149,
        "|",
        "b",
        false,
        false,
        829,
        829,
        829,
        0,
        0,
        "macaddr_or",
        "-",
        "-",
    ),
    (
        3211,
        "->",
        "b",
        false,
        false,
        3802,
        25,
        3802,
        0,
        0,
        "jsonb_object_field",
        "-",
        "-",
    ),
    (
        3212,
        "->",
        "b",
        false,
        false,
        3802,
        23,
        3802,
        0,
        0,
        "jsonb_array_element",
        "-",
        "-",
    ),
    (
        3222,
        "=",
        "b",
        true,
        true,
        3220,
        3220,
        16,
        3222,
        3223,
        "pg_lsn_eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        3223,
        "<>",
        "b",
        false,
        false,
        3220,
        3220,
        16,
        3223,
        3222,
        "pg_lsn_ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        3224,
        "<",
        "b",
        false,
        false,
        3220,
        3220,
        16,
        3225,
        3227,
        "pg_lsn_lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        3225,
        ">",
        "b",
        false,
        false,
        3220,
        3220,
        16,
        3224,
        3226,
        "pg_lsn_gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        3226,
        "<=",
        "b",
        false,
        false,
        3220,
        3220,
        16,
        3227,
        3225,
        "pg_lsn_le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        3227,
        ">=",
        "b",
        false,
        false,
        3220,
        3220,
        16,
        3226,
        3224,
        "pg_lsn_ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        3228,
        "-",
        "b",
        false,
        false,
        3220,
        3220,
        1700,
        0,
        0,
        "pg_lsn_mi",
        "-",
        "-",
    ),
    (
        3240,
        "=",
        "b",
        true,
        true,
        3802,
        3802,
        16,
        3240,
        3241,
        "jsonb_eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        3241,
        "<>",
        "b",
        false,
        false,
        3802,
        3802,
        16,
        3241,
        3240,
        "jsonb_ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        3242,
        "<",
        "b",
        false,
        false,
        3802,
        3802,
        16,
        3243,
        3245,
        "jsonb_lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        3243,
        ">",
        "b",
        false,
        false,
        3802,
        3802,
        16,
        3242,
        3244,
        "jsonb_gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        3244,
        "<=",
        "b",
        false,
        false,
        3802,
        3802,
        16,
        3245,
        3243,
        "jsonb_le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        3245,
        ">=",
        "b",
        false,
        false,
        3802,
        3802,
        16,
        3244,
        3242,
        "jsonb_ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        3246,
        "@>",
        "b",
        false,
        false,
        3802,
        3802,
        16,
        3250,
        0,
        "jsonb_contains",
        "matchingsel",
        "matchingjoinsel",
    ),
    (
        3247,
        "?",
        "b",
        false,
        false,
        3802,
        25,
        16,
        0,
        0,
        "jsonb_exists",
        "matchingsel",
        "matchingjoinsel",
    ),
    (
        3250,
        "<@",
        "b",
        false,
        false,
        3802,
        3802,
        16,
        3246,
        0,
        "jsonb_contained",
        "matchingsel",
        "matchingjoinsel",
    ),
    (
        3276,
        "<->",
        "b",
        false,
        false,
        600,
        604,
        701,
        3289,
        0,
        "dist_ppoly",
        "-",
        "-",
    ),
    (
        3284,
        "||",
        "b",
        false,
        false,
        3802,
        3802,
        3802,
        0,
        0,
        "jsonb_concat",
        "-",
        "-",
    ),
    (
        3285,
        "-",
        "b",
        false,
        false,
        3802,
        25,
        3802,
        0,
        0,
        "pg_catalog.jsonb_delete",
        "-",
        "-",
    ),
    (
        3289,
        "<->",
        "b",
        false,
        false,
        604,
        600,
        701,
        3276,
        0,
        "dist_polyp",
        "-",
        "-",
    ),
    (
        3291,
        "<->",
        "b",
        false,
        false,
        718,
        600,
        701,
        1522,
        0,
        "dist_cpoint",
        "-",
        "-",
    ),
    (
        3315,
        "<>",
        "b",
        false,
        false,
        28,
        28,
        16,
        3315,
        352,
        "xidneq",
        "neqsel",
        "neqjoinsel",
    ),
    (
        3316,
        "<>",
        "b",
        false,
        false,
        28,
        23,
        16,
        0,
        353,
        "xidneqint4",
        "neqsel",
        "neqjoinsel",
    ),
    (
        3346,
        "*",
        "b",
        false,
        false,
        790,
        20,
        790,
        3349,
        0,
        "cash_mul_int8",
        "-",
        "-",
    ),
    (
        3347,
        "/",
        "b",
        false,
        false,
        790,
        20,
        790,
        0,
        0,
        "cash_div_int8",
        "-",
        "-",
    ),
    (
        3349,
        "*",
        "b",
        false,
        false,
        20,
        790,
        790,
        3346,
        0,
        "int8_mul_cash",
        "-",
        "-",
    ),
    (
        3362,
        "=",
        "b",
        true,
        true,
        774,
        774,
        16,
        3362,
        3363,
        "macaddr8_eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        3363,
        "<>",
        "b",
        false,
        false,
        774,
        774,
        16,
        3363,
        3362,
        "macaddr8_ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        3364,
        "<",
        "b",
        false,
        false,
        774,
        774,
        16,
        3366,
        3367,
        "macaddr8_lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        3365,
        "<=",
        "b",
        false,
        false,
        774,
        774,
        16,
        3367,
        3366,
        "macaddr8_le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        3366,
        ">",
        "b",
        false,
        false,
        774,
        774,
        16,
        3364,
        3365,
        "macaddr8_gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        3367,
        ">=",
        "b",
        false,
        false,
        774,
        774,
        16,
        3365,
        3364,
        "macaddr8_ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        3368,
        "~",
        "l",
        false,
        false,
        0,
        774,
        774,
        0,
        0,
        "macaddr8_not",
        "-",
        "-",
    ),
    (
        3369,
        "&",
        "b",
        false,
        false,
        774,
        774,
        774,
        0,
        0,
        "macaddr8_and",
        "-",
        "-",
    ),
    (
        3370,
        "|",
        "b",
        false,
        false,
        774,
        774,
        774,
        0,
        0,
        "macaddr8_or",
        "-",
        "-",
    ),
    (
        3477,
        "->>",
        "b",
        false,
        false,
        3802,
        25,
        25,
        0,
        0,
        "jsonb_object_field_text",
        "-",
        "-",
    ),
    (
        3481,
        "->>",
        "b",
        false,
        false,
        3802,
        23,
        25,
        0,
        0,
        "jsonb_array_element_text",
        "-",
        "-",
    ),
    (
        3516,
        "=",
        "b",
        true,
        true,
        3500,
        3500,
        16,
        3516,
        3517,
        "enum_eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        3517,
        "<>",
        "b",
        false,
        false,
        3500,
        3500,
        16,
        3517,
        3516,
        "enum_ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        3518,
        "<",
        "b",
        false,
        false,
        3500,
        3500,
        16,
        3519,
        3521,
        "enum_lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        3519,
        ">",
        "b",
        false,
        false,
        3500,
        3500,
        16,
        3518,
        3520,
        "enum_gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        3520,
        "<=",
        "b",
        false,
        false,
        3500,
        3500,
        16,
        3521,
        3519,
        "enum_le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        3521,
        ">=",
        "b",
        false,
        false,
        3500,
        3500,
        16,
        3520,
        3518,
        "enum_ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        3552,
        "&&",
        "b",
        false,
        false,
        869,
        869,
        16,
        3552,
        0,
        "network_overlap",
        "networksel",
        "networkjoinsel",
    ),
    (
        3585,
        "&>",
        "b",
        false,
        false,
        3831,
        4537,
        16,
        0,
        0,
        "range_overright_multirange",
        "multirangesel",
        "scalargtjoinsel",
    ),
    (
        3627,
        "<",
        "b",
        false,
        false,
        3614,
        3614,
        16,
        3632,
        3631,
        "tsvector_lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        3628,
        "<=",
        "b",
        false,
        false,
        3614,
        3614,
        16,
        3631,
        3632,
        "tsvector_le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        3629,
        "=",
        "b",
        true,
        false,
        3614,
        3614,
        16,
        3629,
        3630,
        "tsvector_eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        3630,
        "<>",
        "b",
        false,
        false,
        3614,
        3614,
        16,
        3630,
        3629,
        "tsvector_ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        3631,
        ">=",
        "b",
        false,
        false,
        3614,
        3614,
        16,
        3628,
        3627,
        "tsvector_ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        3632,
        ">",
        "b",
        false,
        false,
        3614,
        3614,
        16,
        3627,
        3628,
        "tsvector_gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        3633,
        "||",
        "b",
        false,
        false,
        3614,
        3614,
        3614,
        0,
        0,
        "tsvector_concat",
        "-",
        "-",
    ),
    (
        3636,
        "@@",
        "b",
        false,
        false,
        3614,
        3615,
        16,
        3637,
        0,
        "ts_match_vq",
        "tsmatchsel",
        "tsmatchjoinsel",
    ),
    (
        3637,
        "@@",
        "b",
        false,
        false,
        3615,
        3614,
        16,
        3636,
        0,
        "ts_match_qv",
        "tsmatchsel",
        "tsmatchjoinsel",
    ),
    (
        3660,
        "@@@",
        "b",
        false,
        false,
        3614,
        3615,
        16,
        3661,
        0,
        "ts_match_vq",
        "tsmatchsel",
        "tsmatchjoinsel",
    ),
    (
        3661,
        "@@@",
        "b",
        false,
        false,
        3615,
        3614,
        16,
        3660,
        0,
        "ts_match_qv",
        "tsmatchsel",
        "tsmatchjoinsel",
    ),
    (
        3674,
        "<",
        "b",
        false,
        false,
        3615,
        3615,
        16,
        3679,
        3678,
        "tsquery_lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        3675,
        "<=",
        "b",
        false,
        false,
        3615,
        3615,
        16,
        3678,
        3679,
        "tsquery_le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        3676,
        "=",
        "b",
        true,
        false,
        3615,
        3615,
        16,
        3676,
        3677,
        "tsquery_eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        3677,
        "<>",
        "b",
        false,
        false,
        3615,
        3615,
        16,
        3677,
        3676,
        "tsquery_ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        3678,
        ">=",
        "b",
        false,
        false,
        3615,
        3615,
        16,
        3675,
        3674,
        "tsquery_ge",
        "scalargesel",
        "scalargejoinsel",
    ),
    (
        3679,
        ">",
        "b",
        false,
        false,
        3615,
        3615,
        16,
        3674,
        3675,
        "tsquery_gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        3680,
        "&&",
        "b",
        false,
        false,
        3615,
        3615,
        3615,
        0,
        0,
        "tsquery_and",
        "-",
        "-",
    ),
    (
        3681,
        "||",
        "b",
        false,
        false,
        3615,
        3615,
        3615,
        0,
        0,
        "tsquery_or",
        "-",
        "-",
    ),
    (
        3682,
        "!!",
        "l",
        false,
        false,
        0,
        3615,
        3615,
        0,
        0,
        "tsquery_not",
        "-",
        "-",
    ),
    (
        3693,
        "@>",
        "b",
        false,
        false,
        3615,
        3615,
        16,
        3694,
        0,
        "tsq_mcontains",
        "matchingsel",
        "matchingjoinsel",
    ),
    (
        3694,
        "<@",
        "b",
        false,
        false,
        3615,
        3615,
        16,
        3693,
        0,
        "tsq_mcontained",
        "matchingsel",
        "matchingjoinsel",
    ),
    (
        3825,
        "/",
        "b",
        false,
        false,
        790,
        790,
        701,
        0,
        0,
        "cash_div_cash",
        "-",
        "-",
    ),
    (
        3877,
        "^@",
        "b",
        false,
        false,
        25,
        25,
        16,
        0,
        0,
        "starts_with",
        "prefixsel",
        "prefixjoinsel",
    ),
    (
        3882,
        "=",
        "b",
        true,
        true,
        3831,
        3831,
        16,
        3882,
        3883,
        "range_eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        3883,
        "<>",
        "b",
        false,
        false,
        3831,
        3831,
        16,
        3883,
        3882,
        "range_ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        3884,
        "<",
        "b",
        false,
        false,
        3831,
        3831,
        16,
        3887,
        3886,
        "range_lt",
        "rangesel",
        "scalarltjoinsel",
    ),
    (
        3885,
        "<=",
        "b",
        false,
        false,
        3831,
        3831,
        16,
        3886,
        3887,
        "range_le",
        "rangesel",
        "scalarlejoinsel",
    ),
    (
        3886,
        ">=",
        "b",
        false,
        false,
        3831,
        3831,
        16,
        3885,
        3884,
        "range_ge",
        "rangesel",
        "scalargejoinsel",
    ),
    (
        3887,
        ">",
        "b",
        false,
        false,
        3831,
        3831,
        16,
        3884,
        3885,
        "range_gt",
        "rangesel",
        "scalargtjoinsel",
    ),
    (
        3888,
        "&&",
        "b",
        false,
        false,
        3831,
        3831,
        16,
        3888,
        0,
        "range_overlaps",
        "rangesel",
        "areajoinsel",
    ),
    (
        3889,
        "@>",
        "b",
        false,
        false,
        3831,
        2283,
        16,
        3891,
        0,
        "range_contains_elem",
        "rangesel",
        "contjoinsel",
    ),
    (
        3890,
        "@>",
        "b",
        false,
        false,
        3831,
        3831,
        16,
        3892,
        0,
        "range_contains",
        "rangesel",
        "contjoinsel",
    ),
    (
        3891,
        "<@",
        "b",
        false,
        false,
        2283,
        3831,
        16,
        3889,
        0,
        "elem_contained_by_range",
        "rangesel",
        "contjoinsel",
    ),
    (
        3892,
        "<@",
        "b",
        false,
        false,
        3831,
        3831,
        16,
        3890,
        0,
        "range_contained_by",
        "rangesel",
        "contjoinsel",
    ),
    (
        3893,
        "<<",
        "b",
        false,
        false,
        3831,
        3831,
        16,
        3894,
        0,
        "range_before",
        "rangesel",
        "scalarltjoinsel",
    ),
    (
        3894,
        ">>",
        "b",
        false,
        false,
        3831,
        3831,
        16,
        3893,
        0,
        "range_after",
        "rangesel",
        "scalargtjoinsel",
    ),
    (
        3895,
        "&<",
        "b",
        false,
        false,
        3831,
        3831,
        16,
        0,
        0,
        "range_overleft",
        "rangesel",
        "scalarltjoinsel",
    ),
    (
        3896,
        "&>",
        "b",
        false,
        false,
        3831,
        3831,
        16,
        0,
        0,
        "range_overright",
        "rangesel",
        "scalargtjoinsel",
    ),
    (
        3897,
        "-|-",
        "b",
        false,
        false,
        3831,
        3831,
        16,
        3897,
        0,
        "range_adjacent",
        "matchingsel",
        "matchingjoinsel",
    ),
    (
        3898,
        "+",
        "b",
        false,
        false,
        3831,
        3831,
        3831,
        3898,
        0,
        "range_union",
        "-",
        "-",
    ),
    (
        3899,
        "-",
        "b",
        false,
        false,
        3831,
        3831,
        3831,
        0,
        0,
        "range_minus",
        "-",
        "-",
    ),
    (
        3900,
        "*",
        "b",
        false,
        false,
        3831,
        3831,
        3831,
        3900,
        0,
        "range_intersect",
        "-",
        "-",
    ),
    (
        3962,
        "->",
        "b",
        false,
        false,
        114,
        25,
        114,
        0,
        0,
        "json_object_field",
        "-",
        "-",
    ),
    (
        3963,
        "->>",
        "b",
        false,
        false,
        114,
        25,
        25,
        0,
        0,
        "json_object_field_text",
        "-",
        "-",
    ),
    (
        3964,
        "->",
        "b",
        false,
        false,
        114,
        23,
        114,
        0,
        0,
        "json_array_element",
        "-",
        "-",
    ),
    (
        3965,
        "->>",
        "b",
        false,
        false,
        114,
        23,
        25,
        0,
        0,
        "json_array_element_text",
        "-",
        "-",
    ),
    (
        4012,
        "@?",
        "b",
        false,
        false,
        3802,
        4072,
        16,
        0,
        0,
        "jsonb_path_exists_opr",
        "matchingsel",
        "matchingjoinsel",
    ),
    (
        4035,
        "&>",
        "b",
        false,
        false,
        4537,
        3831,
        16,
        0,
        0,
        "multirange_overright_range",
        "multirangesel",
        "scalargtjoinsel",
    ),
    (
        4142,
        "&>",
        "b",
        false,
        false,
        4537,
        4537,
        16,
        0,
        0,
        "multirange_overright_multirange",
        "multirangesel",
        "scalargtjoinsel",
    ),
    (
        4179,
        "-|-",
        "b",
        false,
        false,
        3831,
        4537,
        16,
        4180,
        0,
        "range_adjacent_multirange",
        "matchingsel",
        "matchingjoinsel",
    ),
    (
        4180,
        "-|-",
        "b",
        false,
        false,
        4537,
        3831,
        16,
        4179,
        0,
        "multirange_adjacent_range",
        "matchingsel",
        "matchingjoinsel",
    ),
    (
        4198,
        "-|-",
        "b",
        false,
        false,
        4537,
        4537,
        16,
        4198,
        0,
        "multirange_adjacent_multirange",
        "matchingsel",
        "matchingjoinsel",
    ),
    (
        4392,
        "+",
        "b",
        false,
        false,
        4537,
        4537,
        4537,
        4392,
        0,
        "multirange_union",
        "-",
        "-",
    ),
    (
        4393,
        "-",
        "b",
        false,
        false,
        4537,
        4537,
        4537,
        0,
        0,
        "multirange_minus",
        "-",
        "-",
    ),
    (
        4394,
        "*",
        "b",
        false,
        false,
        4537,
        4537,
        4537,
        4394,
        0,
        "multirange_intersect",
        "-",
        "-",
    ),
    (
        4395,
        "<<",
        "b",
        false,
        false,
        3831,
        4537,
        16,
        4399,
        0,
        "range_before_multirange",
        "multirangesel",
        "scalarltjoinsel",
    ),
    (
        4396,
        "<<",
        "b",
        false,
        false,
        4537,
        3831,
        16,
        4398,
        0,
        "multirange_before_range",
        "multirangesel",
        "scalarltjoinsel",
    ),
    (
        4397,
        "<<",
        "b",
        false,
        false,
        4537,
        4537,
        16,
        4400,
        0,
        "multirange_before_multirange",
        "multirangesel",
        "scalarltjoinsel",
    ),
    (
        4398,
        ">>",
        "b",
        false,
        false,
        3831,
        4537,
        16,
        4396,
        0,
        "range_after_multirange",
        "multirangesel",
        "scalargtjoinsel",
    ),
    (
        4399,
        ">>",
        "b",
        false,
        false,
        4537,
        3831,
        16,
        4395,
        0,
        "multirange_after_range",
        "multirangesel",
        "scalargtjoinsel",
    ),
    (
        4400,
        ">>",
        "b",
        false,
        false,
        4537,
        4537,
        16,
        4397,
        0,
        "multirange_after_multirange",
        "multirangesel",
        "scalargtjoinsel",
    ),
    (
        4539,
        "@>",
        "b",
        false,
        false,
        3831,
        4537,
        16,
        4540,
        0,
        "range_contains_multirange",
        "multirangesel",
        "contjoinsel",
    ),
    (
        4540,
        "<@",
        "b",
        false,
        false,
        4537,
        3831,
        16,
        4539,
        0,
        "multirange_contained_by_range",
        "multirangesel",
        "contjoinsel",
    ),
    (
        5005,
        "<->",
        "b",
        false,
        false,
        3615,
        3615,
        3615,
        0,
        0,
        "pg_catalog.tsquery_phrase",
        "-",
        "-",
    ),
    (
        5025,
        "+",
        "b",
        false,
        false,
        3220,
        1700,
        3220,
        5026,
        0,
        "pg_lsn_pli",
        "-",
        "-",
    ),
    (
        5026,
        "+",
        "b",
        false,
        false,
        1700,
        3220,
        3220,
        5025,
        0,
        "numeric_pl_pg_lsn",
        "-",
        "-",
    ),
    (
        5068,
        "=",
        "b",
        true,
        true,
        5069,
        5069,
        16,
        5068,
        5072,
        "xid8eq",
        "eqsel",
        "eqjoinsel",
    ),
    (
        5072,
        "<>",
        "b",
        false,
        false,
        5069,
        5069,
        16,
        5072,
        5068,
        "xid8ne",
        "neqsel",
        "neqjoinsel",
    ),
    (
        5073,
        "<",
        "b",
        false,
        false,
        5069,
        5069,
        16,
        5074,
        5076,
        "xid8lt",
        "scalarltsel",
        "scalarltjoinsel",
    ),
    (
        5074,
        ">",
        "b",
        false,
        false,
        5069,
        5069,
        16,
        5073,
        5075,
        "xid8gt",
        "scalargtsel",
        "scalargtjoinsel",
    ),
    (
        5075,
        "<=",
        "b",
        false,
        false,
        5069,
        5069,
        16,
        5076,
        5074,
        "xid8le",
        "scalarlesel",
        "scalarlejoinsel",
    ),
    (
        5076,
        ">=",
        "b",
        false,
        false,
        5069,
        5069,
        16,
        5075,
        5073,
        "xid8ge",
        "scalargesel",
        "scalargejoinsel",
    ),
];

pub(crate) fn synth_pg_operator(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("oprname", DataType::Name, false),
        ColumnSchema::new("oprnamespace", DataType::Oid, false),
        ColumnSchema::new("oprowner", DataType::Oid, false),
        ColumnSchema::new("oprkind", DataType::Char1, false),
        ColumnSchema::new("oprcanmerge", DataType::Bool, false),
        ColumnSchema::new("oprcanhash", DataType::Bool, false),
        ColumnSchema::new("oprleft", DataType::Oid, false),
        ColumnSchema::new("oprright", DataType::Oid, false),
        ColumnSchema::new("oprresult", DataType::Oid, false),
        ColumnSchema::new("oprcom", DataType::Oid, false),
        ColumnSchema::new("oprnegate", DataType::Oid, false),
        ColumnSchema::new("oprcode", DataType::RegProc, false),
        ColumnSchema::new("oprrest", DataType::RegProc, false),
        ColumnSchema::new("oprjoin", DataType::RegProc, false),
    ];
    // 9.0.0 (N22) — PostgreSQL 18.6's own rows, for every operator SPG
    // EVALUATES.
    //
    // This was GENERATED: same-type comparisons over one type list,
    // same-type arithmetic over another, oids counting up from 70,000
    // and `oprcode` the literal `-`. It listed 330 rows where PG has
    // 799, and none of its oids, commutators or negators were the ones
    // a client resolves — so `oprcom` / `oprnegate` joined to nothing.
    //
    // Which rows to carry was MEASURED, not guessed: each of PG's 799
    // was probed over the wire as `null::L <op> null::R`, and the 689
    // SPG answers are in `PG_OPERATORS`. A shape the probe cannot spell
    // leaves its row out, which under-reports — the safe direction, and
    // the opposite of listing an operator the engine cannot apply.
    let mut rows: Vec<Row<'static>> = PG_OPERATORS
        .iter()
        .map(
            |&(
                oid,
                name,
                kind,
                canmerge,
                canhash,
                left,
                right,
                result,
                com,
                negate,
                code,
                rest,
                join,
            )| {
                Row::new(alloc::vec![
                    Value::BigInt(oid),
                    Value::text::<String>(name.into()),
                    Value::BigInt(11), // oprnamespace — pg_catalog
                    Value::BigInt(10), // oprowner
                    char1(kind),
                    Value::Bool(canmerge),
                    Value::Bool(canhash),
                    Value::BigInt(left),
                    Value::BigInt(right),
                    Value::BigInt(result),
                    Value::BigInt(com),
                    Value::BigInt(negate),
                    Value::RegProc(0, code.into()),
                    Value::RegProc(0, rest.into()),
                    Value::RegProc(0, join.into()),
                ])
            },
        )
        .collect();
    // The extension's own operators keep counting from a band of their
    // own, above every oid PostgreSQL ships.
    let mut oid: i64 = 70_000;
    // 9.0.0 — pg_trgm's ten, over two texts, once the extension is
    // installed. They live in the schema it was installed into, not in
    // `pg_catalog`, so they are pushed here rather than through `push`.
    if cat
        .extensions()
        .keys()
        .any(|k| k.eq_ignore_ascii_case("pg_trgm"))
    {
        for (name, result) in [
            ("%", 16i64),
            ("<%", 16),
            ("%>", 16),
            ("<<%", 16),
            ("%>>", 16),
            ("<->", 700),
            ("<<->", 700),
            ("<->>", 700),
            ("<<<->", 700),
            ("<->>>", 700),
        ] {
            oid += 1;
            rows.push(Row::new(alloc::vec![
                Value::BigInt(oid),
                Value::text::<String>(name.into()),
                Value::BigInt(2200), // oprnamespace — the extension's schema
                Value::BigInt(10),   // oprowner
                char1("b"),
                Value::Bool(false),
                Value::Bool(false),
                Value::BigInt(25), // text
                Value::BigInt(25), // text
                Value::BigInt(result),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::RegProc(0, "-".into()),
                Value::RegProc(0, "-".into()),
                Value::RegProc(0, "-".into()),
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
/// 9.0.0 — the I/O functions each type names, read off PostgreSQL
/// 18.6. `pg_type.typinput` / `typoutput` / `typreceive` / `typsend`
/// all read `-` (oid 0, PG's spelling for "no function"), because
/// SPG's I/O is built into the engine and there was no catalogued
/// function to name. The functions are in `PG_PROC_FUNCS` now, so a
/// `pg_type JOIN pg_proc ON proname = typinput::text` resolves, which
/// is what the old comment said naming one without them would break.
///
/// `-` in a slot is PostgreSQL's own answer: a pseudo-type has no
/// binary I/O.
const PG_TYPE_IO: &[(i64, &str, &str, &str, &str)] = &[
    (269, "table_am_handler_in", "table_am_handler_out", "-", "-"),
    (325, "index_am_handler_in", "index_am_handler_out", "-", "-"),
    (
        2275,
        "cstring_in",
        "cstring_out",
        "cstring_recv",
        "cstring_send",
    ),
    (2211, "array_in", "array_out", "array_recv", "array_send"),
    (16, "boolin", "boolout", "boolrecv", "boolsend"),
    (17, "byteain", "byteaout", "bytearecv", "byteasend"),
    (18, "charin", "charout", "charrecv", "charsend"),
    (19, "namein", "nameout", "namerecv", "namesend"),
    (20, "int8in", "int8out", "int8recv", "int8send"),
    (21, "int2in", "int2out", "int2recv", "int2send"),
    (
        22,
        "int2vectorin",
        "int2vectorout",
        "int2vectorrecv",
        "int2vectorsend",
    ),
    (23, "int4in", "int4out", "int4recv", "int4send"),
    (24, "regprocin", "regprocout", "regprocrecv", "regprocsend"),
    (25, "textin", "textout", "textrecv", "textsend"),
    (26, "oidin", "oidout", "oidrecv", "oidsend"),
    (27, "tidin", "tidout", "tidrecv", "tidsend"),
    (28, "xidin", "xidout", "xidrecv", "xidsend"),
    (29, "cidin", "cidout", "cidrecv", "cidsend"),
    (
        30,
        "oidvectorin",
        "oidvectorout",
        "oidvectorrecv",
        "oidvectorsend",
    ),
    (114, "json_in", "json_out", "json_recv", "json_send"),
    (142, "xml_in", "xml_out", "xml_recv", "xml_send"),
    (143, "array_in", "array_out", "array_recv", "array_send"),
    (
        194,
        "pg_node_tree_in",
        "pg_node_tree_out",
        "pg_node_tree_recv",
        "pg_node_tree_send",
    ),
    (199, "array_in", "array_out", "array_recv", "array_send"),
    (600, "point_in", "point_out", "point_recv", "point_send"),
    (650, "cidr_in", "cidr_out", "cidr_recv", "cidr_send"),
    (651, "array_in", "array_out", "array_recv", "array_send"),
    (700, "float4in", "float4out", "float4recv", "float4send"),
    (701, "float8in", "float8out", "float8recv", "float8send"),
    (
        774,
        "macaddr8_in",
        "macaddr8_out",
        "macaddr8_recv",
        "macaddr8_send",
    ),
    (790, "cash_in", "cash_out", "cash_recv", "cash_send"),
    (791, "array_in", "array_out", "array_recv", "array_send"),
    (
        829,
        "macaddr_in",
        "macaddr_out",
        "macaddr_recv",
        "macaddr_send",
    ),
    (869, "inet_in", "inet_out", "inet_recv", "inet_send"),
    (1000, "array_in", "array_out", "array_recv", "array_send"),
    (1001, "array_in", "array_out", "array_recv", "array_send"),
    (1002, "array_in", "array_out", "array_recv", "array_send"),
    (1003, "array_in", "array_out", "array_recv", "array_send"),
    (1005, "array_in", "array_out", "array_recv", "array_send"),
    (1007, "array_in", "array_out", "array_recv", "array_send"),
    (1008, "array_in", "array_out", "array_recv", "array_send"),
    (1009, "array_in", "array_out", "array_recv", "array_send"),
    (1014, "array_in", "array_out", "array_recv", "array_send"),
    (1015, "array_in", "array_out", "array_recv", "array_send"),
    (1016, "array_in", "array_out", "array_recv", "array_send"),
    (1021, "array_in", "array_out", "array_recv", "array_send"),
    (1022, "array_in", "array_out", "array_recv", "array_send"),
    (1028, "array_in", "array_out", "array_recv", "array_send"),
    (1033, "aclitemin", "aclitemout", "-", "-"),
    (1034, "array_in", "array_out", "array_recv", "array_send"),
    (1040, "array_in", "array_out", "array_recv", "array_send"),
    (1041, "array_in", "array_out", "array_recv", "array_send"),
    (1042, "bpcharin", "bpcharout", "bpcharrecv", "bpcharsend"),
    (
        1043,
        "varcharin",
        "varcharout",
        "varcharrecv",
        "varcharsend",
    ),
    (1082, "date_in", "date_out", "date_recv", "date_send"),
    (1083, "time_in", "time_out", "time_recv", "time_send"),
    (
        1114,
        "timestamp_in",
        "timestamp_out",
        "timestamp_recv",
        "timestamp_send",
    ),
    (1115, "array_in", "array_out", "array_recv", "array_send"),
    (1182, "array_in", "array_out", "array_recv", "array_send"),
    (1183, "array_in", "array_out", "array_recv", "array_send"),
    (
        1184,
        "timestamptz_in",
        "timestamptz_out",
        "timestamptz_recv",
        "timestamptz_send",
    ),
    (1185, "array_in", "array_out", "array_recv", "array_send"),
    (
        1186,
        "interval_in",
        "interval_out",
        "interval_recv",
        "interval_send",
    ),
    (1187, "array_in", "array_out", "array_recv", "array_send"),
    (1231, "array_in", "array_out", "array_recv", "array_send"),
    (
        1266,
        "timetz_in",
        "timetz_out",
        "timetz_recv",
        "timetz_send",
    ),
    (1270, "array_in", "array_out", "array_recv", "array_send"),
    (1560, "bit_in", "bit_out", "bit_recv", "bit_send"),
    (
        1562,
        "varbit_in",
        "varbit_out",
        "varbit_recv",
        "varbit_send",
    ),
    (
        1700,
        "numeric_in",
        "numeric_out",
        "numeric_recv",
        "numeric_send",
    ),
    (
        2205,
        "regclassin",
        "regclassout",
        "regclassrecv",
        "regclasssend",
    ),
    (
        2206,
        "regtypein",
        "regtypeout",
        "regtyperecv",
        "regtypesend",
    ),
    (
        2249,
        "record_in",
        "record_out",
        "record_recv",
        "record_send",
    ),
    (
        2277,
        "anyarray_in",
        "anyarray_out",
        "anyarray_recv",
        "anyarray_send",
    ),
    (2278, "void_in", "void_out", "void_recv", "void_send"),
    (2279, "trigger_in", "trigger_out", "-", "-"),
    (2283, "anyelement_in", "anyelement_out", "-", "-"),
    (2950, "uuid_in", "uuid_out", "uuid_recv", "uuid_send"),
    (2951, "array_in", "array_out", "array_recv", "array_send"),
    (
        3220,
        "pg_lsn_in",
        "pg_lsn_out",
        "pg_lsn_recv",
        "pg_lsn_send",
    ),
    (
        3614,
        "tsvectorin",
        "tsvectorout",
        "tsvectorrecv",
        "tsvectorsend",
    ),
    (
        3615,
        "tsqueryin",
        "tsqueryout",
        "tsqueryrecv",
        "tsquerysend",
    ),
    (3643, "array_in", "array_out", "array_recv", "array_send"),
    (3645, "array_in", "array_out", "array_recv", "array_send"),
    (3802, "jsonb_in", "jsonb_out", "jsonb_recv", "jsonb_send"),
    (3807, "array_in", "array_out", "array_recv", "array_send"),
    (3831, "anyrange_in", "anyrange_out", "-", "-"),
    (3904, "range_in", "range_out", "range_recv", "range_send"),
    (3906, "range_in", "range_out", "range_recv", "range_send"),
    (3908, "range_in", "range_out", "range_recv", "range_send"),
    (3910, "range_in", "range_out", "range_recv", "range_send"),
    (3912, "range_in", "range_out", "range_recv", "range_send"),
    (3926, "range_in", "range_out", "range_recv", "range_send"),
    (
        4451,
        "multirange_in",
        "multirange_out",
        "multirange_recv",
        "multirange_send",
    ),
    (
        4532,
        "multirange_in",
        "multirange_out",
        "multirange_recv",
        "multirange_send",
    ),
    (
        4533,
        "multirange_in",
        "multirange_out",
        "multirange_recv",
        "multirange_send",
    ),
    (
        4534,
        "multirange_in",
        "multirange_out",
        "multirange_recv",
        "multirange_send",
    ),
    (
        4535,
        "multirange_in",
        "multirange_out",
        "multirange_recv",
        "multirange_send",
    ),
    (
        4536,
        "multirange_in",
        "multirange_out",
        "multirange_recv",
        "multirange_send",
    ),
    (4537, "anymultirange_in", "anymultirange_out", "-", "-"),
    (5069, "xid8in", "xid8out", "xid8recv", "xid8send"),
    (
        5078,
        "anycompatiblearray_in",
        "anycompatiblearray_out",
        "anycompatiblearray_recv",
        "anycompatiblearray_send",
    ),
];

/// 9.0.0 — `(typinput, typoutput, typreceive, typsend)` for a type oid,
/// as `regproc` values. `-` where PostgreSQL has none, and for a type
/// the table does not carry.
fn pg_type_io(
    oid: i64,
) -> (
    Value<'static>,
    Value<'static>,
    Value<'static>,
    Value<'static>,
) {
    let none = || Value::RegProc(0, alloc::string::String::from("-").into_boxed_str());
    let Some((_, i, o, r, s)) = PG_TYPE_IO.iter().find(|(t, ..)| *t == oid) else {
        return (none(), none(), none(), none());
    };
    // The oid is looked up in the same table the catalog publishes, so a
    // name here and a `pg_proc` row there cannot disagree.
    let proc_of = |name: &str| -> Value<'static> {
        if name == "-" {
            return none();
        }
        let oid = PG_PROC_FUNCS
            .iter()
            .find(|(_, n, ..)| *n == name)
            .map_or(0, |(o, ..)| *o);
        Value::RegProc(oid, alloc::string::String::from(name).into_boxed_str())
    };
    (proc_of(i), proc_of(o), proc_of(r), proc_of(s))
}

pub(crate) fn synth_pg_type(
    cat: &Catalog,
    roles: &crate::role_directory::RoleDirectory,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    // v7.37.24 (24.7) — widened from 8 to 16 PG-canonical columns.
    // ORMs / monitoring tools query typbyval / typispreferred /
    // typdelim / typisdefined to decide encoding strategies; the
    // shape matches PG exactly so introspection round-trips
    // succeed.
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("typname", DataType::Name, false),
        ColumnSchema::new("typnamespace", DataType::Oid, false),
        ColumnSchema::new("typowner", DataType::Oid, false),
        ColumnSchema::new("typlen", DataType::SmallInt, false),
        ColumnSchema::new("typbyval", DataType::Bool, false),
        ColumnSchema::new("typtype", DataType::Char1, false),
        ColumnSchema::new("typcategory", DataType::Char1, false),
        ColumnSchema::new("typispreferred", DataType::Bool, false),
        ColumnSchema::new("typisdefined", DataType::Bool, false),
        ColumnSchema::new("typdelim", DataType::Char1, false),
        ColumnSchema::new("typrelid", DataType::Oid, false),
        ColumnSchema::new("typsubscript", DataType::RegProc, false),
        ColumnSchema::new("typelem", DataType::Oid, false),
        ColumnSchema::new("typarray", DataType::Oid, false),
        // v7.39 (round 543) — PG names the type's I/O functions here.
        // SPG's I/O is built into the engine and is not a catalogued
        // function, so there is no pg_proc row to point at; naming one
        // would leave `pg_type JOIN pg_proc ON proname = typinput::text`
        // dangling. 0 is the value PG itself uses for a type with no
        // such function — which is what typmodin/typmodout/typanalyze
        // read on PG for int4 and text, measured.
        ColumnSchema::new("typinput", DataType::RegProc, false),
        ColumnSchema::new("typoutput", DataType::RegProc, false),
        ColumnSchema::new("typreceive", DataType::RegProc, false),
        ColumnSchema::new("typsend", DataType::RegProc, false),
        ColumnSchema::new("typmodin", DataType::RegProc, false),
        ColumnSchema::new("typmodout", DataType::RegProc, false),
        ColumnSchema::new("typanalyze", DataType::RegProc, false),
        ColumnSchema::new("typalign", DataType::Char1, false),
        ColumnSchema::new("typstorage", DataType::Char1, false),
        ColumnSchema::new("typnotnull", DataType::Bool, false),
        ColumnSchema::new("typbasetype", DataType::Oid, false),
        ColumnSchema::new("typtypmod", DataType::Int, false),
        ColumnSchema::new("typndims", DataType::Int, false),
        ColumnSchema::new("typcollation", DataType::Oid, false),
        // PG's last three; `pg_dump` selects typacl by name.
        ColumnSchema::new("typdefaultbin", DataType::PgNodeTree, true),
        ColumnSchema::new("typdefault", DataType::Text, true),
        ColumnSchema::new("typacl", DataType::AclItemArray, true),
    ];
    let scalars = PG_SCALAR_TYPES;
    // Array companion types share the typelem / typcategory='A'.
    // We emit just the array OIDs the scalars reference.
    let arrays: &[(i64, &str, i64)] = ARRAY_TYPE_OIDS;
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
        // 9.0.0 — the four I/O functions this type names, by oid. A type
        // the table does not carry keeps `-`, which is what every one of
        // them read before.
        let io = pg_type_io(oid);
        // v7.39 (round 640) — `tid` is 6 bytes, which is neither a width
        // a value can be passed in a register at nor one the 1/2/4/8
        // ladder below has a rung for. PG measures it typbyval false,
        // typalign 's'; the derivation would have said true and 'd'.
        // The rule is width-based, not a list of names: an odd width
        // is by-reference and aligns to its largest even divisor.
        let odd_width = len > 0 && !matches!(len, 1 | 2 | 4 | 8);
        let typbyval = len > 0 && len <= 8 && !odd_width;
        let typalign = match len {
            1 => "c",
            2 => "s",
            4 => "i",
            _ if odd_width => "s",
            _ => "d",
        };
        let typstorage = if len > 0 { "p" } else { "x" };
        let typispreferred = preferred_oids.contains(&oid);
        Row::new(alloc::vec![
            Value::BigInt(oid),
            Value::text::<String>(name.into()),
            // 7.38.1 S5.1 — builtin types live in pg_catalog (11).
            // Claiming 'public' made pg_dump treat all ~100 of them as
            // USER-DEFINED base types and try to dump each (first
            // casualty: dumpBaseType on _aclitem, whose '-' regproc
            // fields do not survive an oid cast).
            Value::BigInt(11), // typnamespace
            Value::BigInt(10), // typowner (postgres superuser OID)
            Value::SmallInt(len),
            Value::Bool(typbyval),
            char1(ty),  // typtype
            char1(cat), // typcategory
            Value::Bool(typispreferred),
            Value::Bool(true),             // typisdefined
            char1(","),                    // typdelim
            Value::BigInt(0),              // typrelid (composite-type table OID)
            Value::RegProc(0, "-".into()), // typsubscript
            Value::BigInt(elem),
            // v7.39 (round 640) — typarray names the type's array type,
            // and PG's invariant is that it resolves: zero dangling
            // pointers there, measured. SPG had 16 — `bit` naming 1561,
            // `int4range` naming 3905, one for every type rounds 635 and
            // 638 added — because it listed the scalar and not the array,
            // and it does not HAVE the array (`pg_typeof(NULL::bit[])`
            // is `unknown`). Naming a type nothing carries is the false
            // claim; 0 is what PG itself writes for a type with no array
            // type. The real number comes back when the array type does.
            Value::BigInt(arr),
            // round 543 — the seven I/O-function oids, all 0: SPG's I/O
            // is not a catalogued function, so there is nothing to name.
            // 9.0.0 — four of the seven ARE named now: the functions are
            // in `pg_proc` (see `PG_TYPE_IO`), so `typinput` reads
            // `int4in` as PostgreSQL 18.6 reports it and the join to
            // `pg_proc` resolves. `typmodin` / `typmodout` /
            // `typanalyze` read `-` on PG too for these types, measured.
            io.0,
            io.1,
            io.2,
            io.3,
            Value::RegProc(0, "-".into()), // typmodin
            Value::RegProc(0, "-".into()), // typmodout
            Value::RegProc(0, "-".into()), // typanalyze
            char1(typalign),
            char1(typstorage),
            Value::Bool(false), // typnotnull — base types are nullable
            Value::BigInt(0),   // typbasetype (DOMAIN base; 0 for base types)
            Value::Int(-1),     // typtypmod
            Value::Int(0),      // typndims
            Value::BigInt(type_collation(oid)),
            Value::Null, // typdefaultbin
            Value::Null, // typdefault
            Value::Null, // typacl — PG's default: no explicit grant
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
    // v7.39 (round 621) — the user-defined types. `pg_enum` listed an enum's
    // labels while `pg_type` did not list the enum, so the standard
    // `pg_enum JOIN pg_type` came back empty and `WHERE typtype = 'e'` found
    // nothing — for a type that casts, compares and reports itself correctly
    // everywhere else. Composites and domains were absent for the same reason.
    //
    // `typbasetype` carries the domain's base OID, which is what tells a
    // client the domain is over an integer; a composite's `typrelid` stays 0
    // because SPG does not give one a backing relation.
    let (enum_oids, composite_oids, domain_oids) = user_type_oids(cat);
    // 7.38.1 S5.1 — build_row stamps pg_catalog (11) for the builtin
    // types above; USER types live in public (2200) so pg_dump keeps
    // dumping them (CREATE TYPE / CREATE DOMAIN survive the roundtrip).
    let into_public = |mut r: Row<'static>| -> Row<'static> {
        r.values[2] = Value::BigInt(2200);
        r
    };
    // 8.0.3 — and belong to whoever created them.
    let owned = |mut r: Row<'static>, kind: spg_storage::NonTableKind, name: &str| {
        r.values[3] = Value::BigInt(roles.owner_oid(cat.object_owner(kind, name)));
        r
    };
    for (name, oid) in enum_oids {
        let r = into_public(build_row(oid, &name, 4, "e", "E", 0, 0, "-"));
        rows.push(owned(r, spg_storage::NonTableKind::EnumType, &name));
    }
    for (ci, (name, oid)) in composite_oids.into_iter().enumerate() {
        let r = into_public(build_row(oid, &name, -1, "c", "C", 0, 0, "-"));
        let mut r = owned(r, spg_storage::NonTableKind::CompositeType, &name);
        // 7.38.1 S5.2 — typrelid points at the relkind-'c' pg_class
        // row (56_001+ band) whose pg_attribute rows carry the fields;
        // pg_dump reads `CREATE TYPE x AS (…)` from exactly that join.
        r.values[11] = Value::BigInt(56_001 + ci as i64);
        rows.push(r);
    }
    for (name, oid) in domain_oids {
        let base = cat
            .domain_types()
            .get(&name)
            .and_then(|d| pg_type_oid_for_domain_base(d))
            .unwrap_or(0);
        // 8.0.3 — a domain is as long as its base type (PG: `posint` over
        // integer is 4, storage plain).
        let mut r = build_row(oid, &name, type_len_of_oid(cat, base), "d", "N", 0, 0, "-");
        // A domain collates as its base type does.
        if let Some(i) = schema.iter().position(|c| c.name == "typcollation")
            && let Some(slot) = r.values.get_mut(i)
        {
            *slot = Value::BigInt(type_collation(base));
        }
        // Found by NAME, not by a counted index: the first cut wrote to
        // position 20 and landed on `typmodout`, so the domain reported a base
        // of 0 and the test said so.
        if let Some(i) = schema.iter().position(|c| c.name == "typbasetype")
            && let Some(slot) = r.values.get_mut(i)
        {
            *slot = Value::BigInt(base);
        }
        rows.push(owned(
            into_public(r),
            spg_storage::NonTableKind::DomainType,
            &name,
        ));
    }

    (schema, rows)
}

/// 8.0.3 — the length of a catalogued type by oid: a built-in's own, an
/// enum's 4, a domain's base type's, and -1 for a composite or anything
/// variable.
fn type_len_of_oid(cat: &Catalog, oid: i64) -> i16 {
    let (enums, _, domains) = user_type_oids(cat);
    if enums.iter().any(|(_, o)| *o == oid) {
        return 4;
    }
    if let Some((name, _)) = domains.iter().find(|(_, o)| *o == oid) {
        return cat
            .domain_types()
            .get(name)
            .and_then(pg_type_oid_for_domain_base)
            .map_or(-1, |base| type_len_of_oid(cat, base));
    }
    PG_SCALAR_TYPES
        .iter()
        .find(|t| t.0 == oid)
        .map_or(-1, |t| t.2)
}

/// 8.0.3 — a catalogued type's collation by oid; a domain collates as its
/// base type does.
fn type_collation_of_oid(cat: &Catalog, oid: i64) -> i64 {
    let (_, _, domains) = user_type_oids(cat);
    if let Some((name, _)) = domains.iter().find(|(_, o)| *o == oid) {
        return cat
            .domain_types()
            .get(name)
            .and_then(pg_type_oid_for_domain_base)
            .map_or(0, type_collation);
    }
    type_collation(oid)
}

/// 8.0.3 — `pg_type.typcollation`: the default collation (100) for the
/// collatable types, `C` (950) for `name`, 0 for the rest. Measured on
/// PG 18.6 over pg_catalog. It read 0 for every type, so each text column's
/// `attcollation` (100) differed from its type's and `pg_dump` wrote
/// `COLLATE pg_catalog."default"` on every one.
fn type_collation(oid: i64) -> i64 {
    match oid {
        19 | 1003 => 950,
        25 | 194 | 1009 | 1014 | 1015 | 1042 | 1043 | 3361 | 3402 | 4600 | 4601 | 5017 => 100,
        _ => 0,
    }
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
/// v7.39 (round 542) — PG18's nineteen columns.
///
/// This published `relname`, `timing`, `events` and `function` — the
/// column names of `information_schema.triggers`, not pg_catalog's. So
/// the catalog existed under the right name with somebody else's
/// columns, and every pg_catalog trigger query failed with "column
/// does not exist" rather than returning nothing: the author reads
/// that as their SQL being wrong.
///
/// `tgtype` is PG's bitmask, and the whole point of the column — it is
/// how a tool learns BEFORE-vs-AFTER and which events fire without
/// parsing text. Measured on PG18: 1 ROW, 2 BEFORE, 4 INSERT,
/// 8 DELETE, 16 UPDATE, 32 TRUNCATE, 64 INSTEAD OF; a
/// `BEFORE INSERT … FOR EACH ROW` reads 7.
pub(crate) fn synth_pg_trigger(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("tgrelid", DataType::Oid, false),
        ColumnSchema::new("tgparentid", DataType::Oid, false),
        ColumnSchema::new("tgname", DataType::Name, false),
        ColumnSchema::new("tgfoid", DataType::Oid, false),
        ColumnSchema::new("tgtype", DataType::SmallInt, false),
        ColumnSchema::new("tgenabled", DataType::Char1, false),
        ColumnSchema::new("tgisinternal", DataType::Bool, false),
        ColumnSchema::new("tgconstrrelid", DataType::Oid, false),
        ColumnSchema::new("tgconstrindid", DataType::Oid, false),
        ColumnSchema::new("tgconstraint", DataType::Oid, false),
        ColumnSchema::new("tgdeferrable", DataType::Bool, false),
        ColumnSchema::new("tginitdeferred", DataType::Bool, false),
        ColumnSchema::new("tgnargs", DataType::SmallInt, false),
        ColumnSchema::new("tgattr", DataType::Int2Vector, true),
        ColumnSchema::new("tgargs", DataType::Bytes, true),
        ColumnSchema::new("tgqual", DataType::PgNodeTree, true),
        ColumnSchema::new("tgoldtable", DataType::Name, true),
        ColumnSchema::new("tgnewtable", DataType::Name, true),
    ];
    let mut oid = OID_TRIGGER_BASE;
    let rows: Vec<Row<'static>> = cat
        .triggers()
        .iter()
        .map(|t| {
            oid += 1;
            let mut tgtype: i16 = 0;
            if !t.timing.eq_ignore_ascii_case("INSTEAD OF") {
                // SPG's triggers are row-level; PG sets bit 1 for those.
                tgtype |= 1;
            }
            if t.timing.eq_ignore_ascii_case("BEFORE") {
                tgtype |= 2;
            }
            if t.timing.eq_ignore_ascii_case("INSTEAD OF") {
                tgtype |= 64;
            }
            for ev in &t.events {
                tgtype |= match ev.to_ascii_uppercase().as_str() {
                    "INSERT" => 4,
                    "DELETE" => 8,
                    "UPDATE" => 16,
                    "TRUNCATE" => 32,
                    _ => 0,
                };
            }
            Row::new(alloc::vec![
                Value::BigInt(oid),
                Value::BigInt(relation_oid(cat, &t.table).unwrap_or(0)),
                Value::BigInt(0), // tgparentid — SPG has no partition-cloned triggers
                Value::text(t.name.clone()),
                Value::BigInt(function_oid(cat, &t.function).unwrap_or(0)),
                Value::SmallInt(tgtype),
                char1(if t.enabled { "O" } else { "D" }), // tgenabled
                Value::Bool(false),                       // tgisinternal
                Value::BigInt(0),                         // tgconstrrelid
                Value::BigInt(0),                         // tgconstrindid
                Value::BigInt(0),                         // tgconstraint
                Value::Bool(false),                       // tgdeferrable
                Value::Bool(false),                       // tginitdeferred
                Value::SmallInt(0),                       // tgnargs — SPG's triggers take none
                Value::Int2Vector(Vec::new()),            // tgattr — empty, as PG has it
                Value::Bytes(alloc::borrow::Cow::Owned(Vec::new())), // tgargs
                Value::Null,                              // tgqual — no WHEN clause
                Value::Null,                              // tgoldtable
                Value::Null,                              // tgnewtable
            ])
        })
        .collect();
    (schema, rows)
}

pub(crate) fn synth_pg_proc(
    cat: &Catalog,
    roles: &crate::role_directory::RoleDirectory,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    // v7.37.24 (24.6) — widened from 6 to 20 PG-canonical columns
    // covering the function metadata that ORMs (Diesel, sea-orm)
    // and pgAdmin's function browser query: prolang for language
    // dispatch, prosrc as the body excerpt, proretset for SETOF,
    // proisstrict for NULL-handling, provolatile / proparallel /
    // procost for planner annotations.
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("proname", DataType::Name, false),
        ColumnSchema::new("pronamespace", DataType::Oid, false),
        ColumnSchema::new("proowner", DataType::Oid, false),
        ColumnSchema::new("prolang", DataType::Oid, false),
        ColumnSchema::new("procost", DataType::Real, false),
        ColumnSchema::new("prorows", DataType::Real, false),
        ColumnSchema::new("provariadic", DataType::Oid, false),
        // v7.39 (round 543) — a planner-support function; SPG has none.
        // 8.0.3 — a `regproc`, which prints `-` for "none" (measured on
        // 18.6). It read the number 0, and `pg_dump` — which tests the text
        // against "-" — wrote `SUPPORT 0` into every CREATE FUNCTION, a
        // syntax error on restore into PostgreSQL and into SPG alike.
        ColumnSchema::new("prosupport", DataType::RegProc, false),
        ColumnSchema::new("prokind", DataType::Char1, false),
        ColumnSchema::new("prosecdef", DataType::Bool, false),
        ColumnSchema::new("proleakproof", DataType::Bool, false),
        ColumnSchema::new("proisstrict", DataType::Bool, false),
        ColumnSchema::new("proretset", DataType::Bool, false),
        ColumnSchema::new("provolatile", DataType::Char1, false),
        ColumnSchema::new("proparallel", DataType::Char1, false),
        ColumnSchema::new("pronargs", DataType::SmallInt, false),
        ColumnSchema::new("pronargdefaults", DataType::SmallInt, false),
        ColumnSchema::new("prorettype", DataType::Oid, false),
        // v7.39.11 — PG's own type; see the row builder below.
        ColumnSchema::new("proargtypes", DataType::OidVector, false),
        ColumnSchema::new("proallargtypes", DataType::OidArray, true),
        ColumnSchema::new("proargmodes", DataType::Char1Array, true),
        // The one of these five SPG really has: CREATE FUNCTION names
        // its parameters, and every client that offers named-argument
        // calls reads them from here.
        ColumnSchema::new("proargnames", DataType::TextArray, true),
        ColumnSchema::new("proargdefaults", DataType::PgNodeTree, true),
        ColumnSchema::new("protrftypes", DataType::OidArray, true),
        ColumnSchema::new("prosrc", DataType::Text, false),
        ColumnSchema::new("probin", DataType::Text, true),
        ColumnSchema::new("prosqlbody", DataType::PgNodeTree, true),
        ColumnSchema::new("proconfig", DataType::TextArray, true),
        // v7.39 (read01 round 61) — the function ACL. NULL means PG's default:
        // PUBLIC may EXECUTE.
        ColumnSchema::new("proacl", DataType::AclItemArray, true),
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
        // 9.0.0 — a function an EXTENSION supplies is in the catalog once
        // the extension is installed, and not before. It was always
        // listed, so `SELECT count(*) FROM pg_proc WHERE
        // proname = 'similarity'` — the probe a client uses to decide
        // whether pg_trgm is there — answered 1 on a database where
        // CALLING it says it does not exist. Two surfaces, two answers.
        let extension_schema = match crate::extension::supplying_extension(name) {
            None => None,
            Some(ext) => match cat
                .extensions()
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(ext))
            {
                None => continue,
                // Extensions install into `public` here; SPG has one
                // user schema, so that is the oid whatever the name.
                Some(_) => Some(2200),
            },
        };
        let provolatile: &str = if volatile_names.contains(&name) {
            "v"
        } else {
            "i"
        };
        let prorows: f64 = match kind {
            "a" | "w" => 1000.0,
            _ => 0.0,
        };
        // proargtypes — PG's `oidvector`; we don't have the per-arg
        // type list in the table, so synthesise N placeholder zeros.
        // ORMs that need the real list will fall back to the
        // pg_proc.dat columns of pg_proc-loaded extensions.
        //
        // v7.39.11 — a real `oidvector`, so `unnest(proargtypes)` and
        // `= ANY (proargtypes)` reach it; see `pg_index`'s vectors.
        let arg_count = if nargs < 0 { 0 } else { nargs };
        let argtypes: Vec<u32> = alloc::vec![0; usize::try_from(arg_count).unwrap_or(0)];
        rows.push(Row::new(alloc::vec![
            Value::BigInt(oid),
            Value::text::<String>(name.into()),
            // v7.39 (round 661) — `pg_catalog` only for what PG18 really
            // has; SPG's own surface goes to `pg_spg`. 9.0.0 — and an
            // extension's goes to the schema it was installed into, which
            // is where PG 18.6 puts it.
            Value::BigInt(match extension_schema {
                Some(ns) => ns,
                None if SPG_ONLY_PROCS.contains(&name) => 13500,
                None => 11,
            }),
            Value::BigInt(10),             // proowner
            Value::BigInt(12),             // prolang = internal
            Value::Real(1.0),              // procost
            Value::Real(prorows as f32),   // prorows
            Value::BigInt(0),              // provariadic
            Value::RegProc(0, "-".into()), // prosupport
            char1(kind),                   // prokind
            Value::Bool(false),            // prosecdef
            Value::Bool(false),            // proleakproof
            Value::Bool(true),             // proisstrict
            Value::Bool(kind == "w"),      // proretset — window funcs return per-row sets
            char1(provolatile),
            char1("s"), // proparallel = safe
            Value::SmallInt(i16::try_from(nargs.max(0)).unwrap_or(i16::MAX)),
            Value::SmallInt(0), // pronargdefaults
            Value::BigInt(rettype),
            Value::OidVector(argtypes),
            Value::Null,                        // proallargtypes
            Value::Null,                        // proargmodes
            Value::Null,                        // proargnames — a builtin's are not catalogued
            Value::Null,                        // proargdefaults
            Value::Null,                        // protrftypes
            Value::text::<String>(name.into()), // prosrc
            Value::Null,                        // probin
            Value::Null,                        // prosqlbody
            Value::Null,                        // proconfig
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
            Value::BigInt(roles.owner_oid(def.owner.as_deref())),
            // prolang: 14 = sql, 13647 = plpgsql — `pg_language`'s own oids
            // (measured on 18.6). It said 13, which is `c`, and matched no
            // `pg_language` row, so `pg_dump`'s function query joined to
            // nothing and stopped: `query returned 0 rows instead of one`.
            Value::BigInt(if def.language.eq_ignore_ascii_case("plpgsql") {
                13647
            } else {
                14
            }),
            // v7.39 (round 322, V46) — the DECLARED attributes, not a row
            // of PG defaults: `CREATE FUNCTION … IMMUTABLE STRICT` was a
            // parse error until this round, so there was nothing else to
            // report; now there is.
            Value::Real(def.cost.unwrap_or(100.0) as f32), // procost
            Value::Real(def.rows.unwrap_or(0.0) as f32),   // prorows
            Value::BigInt(0),
            Value::RegProc(0, "-".into()), // prosupport
            char1("f"),                    // prokind — a normal function
            Value::Bool(def.security_definer),
            Value::Bool(def.leakproof),
            Value::Bool(def.strict),
            Value::Bool(false),
            Value::Char1(def.volatility), // provolatile
            Value::Char1(def.parallel),   // proparallel
            Value::SmallInt(i16::try_from(nargs).unwrap_or(i16::MAX)),
            Value::SmallInt(0),
            Value::BigInt(0),
            // v7.39.11 — proargtypes is an `oidvector` now, and a
            // user-defined function's row has to carry one too or the
            // column holds two shapes. Placeholder zeros, one per
            // declared argument, as the built-in rows carry.
            Value::OidVector(alloc::vec![0; nargs]),
            Value::Null, // proallargtypes
            Value::Null, // proargmodes
            // v7.39 (round 543) — the declared parameter NAMES, which
            // CREATE FUNCTION records and every named-argument caller
            // reads from here.
            declared_arg_names(&def.args_repr),
            Value::Null,                   // proargdefaults
            Value::Null,                   // protrftypes
            Value::text(def.body.clone()), // prosrc — the real body
            Value::Null,                   // probin
            Value::Null,                   // prosqlbody
            Value::Null,                   // proconfig
            crate::acl::render_acl_list(&def.acl).map_or(Value::Null, Value::text),
        ]));
    }
    (schema, rows)
}

/// (oid, name, kind, nargs, rettype). OIDs taken from PG's pg_proc.dat
/// for the common subset. v7.39 (read01 regproc.c) — module-level so the
/// regproc/regprocedure casts resolve names against the same table
/// pg_proc synthesises.
/// v7.39 (round 661) — the names the engine answers that PG18 does not have.
/// They keep their rows (all 86 are callable — measured) but sit in
/// `pg_spg`, so a client asking "does PostgreSQL provide this?" gets
/// the right answer while a client asking "can I call this?" still finds it.
pub(crate) const SPG_ONLY_PROCS: &[&str] = &[
    "benchmark",
    "connection_id",
    "current_catalog",
    "current_role",
    "database",
    "field",
    "found_rows",
    "from_unixtime",
    "gen_uuid_v7",
    "ifnull",
    "isnull",
    "json_array",
    "last_insert_id",
    "log2",
    "nullif",
    "pg_backend_start_time",
    "pg_current_edition",
    "pg_current_query",
    "pg_get_wait_event_name",
    "pg_get_wait_event_type",
    "pg_is_in_backup",
    "pg_last_xid",
    "pg_object_size",
    "pg_prewarm",
    "pg_relation_size_pretty",
    "pg_rotate_logfile_v2",
    "pg_start_backup",
    "pg_stat_get_archiver_archived_count",
    "pg_stat_get_archiver_failed_count",
    "pg_stat_get_archiver_last_archived_wal",
    "pg_stat_get_archiver_last_failed_wal",
    "pg_stat_get_bgwriter_buf_written_checkpoints",
    "pg_stat_get_bgwriter_requested_checkpoints",
    "pg_stat_get_bgwriter_timed_checkpoints",
    "pg_stat_get_buf_fsync_backend",
    "pg_stat_get_buf_written_backend",
    "pg_stat_get_checkpoint_sync_time",
    "pg_stat_get_checkpoint_write_time",
    "pg_stat_get_idx_scan",
    "pg_stat_get_idx_tup_fetch",
    "pg_stat_get_idx_tup_read",
    "pg_stat_get_recovery_prefetch_reset_time",
    "pg_stat_get_seq_scan",
    "pg_stat_get_seq_scan_pos",
    "pg_stat_get_seq_tup_read",
    "pg_stat_get_slru_blks_exists",
    "pg_stat_get_slru_blks_hit",
    "pg_stat_get_slru_blks_read",
    "pg_stat_get_slru_blks_written",
    "pg_stat_get_slru_blks_zeroed",
    "pg_stat_get_slru_flushes",
    "pg_stat_get_slru_stat_reset_time",
    "pg_stat_get_slru_truncates",
    "pg_stat_get_stat_snapshot_timestamp",
    "pg_stat_get_tid_scan_pos",
    "pg_stat_get_wal_buffers_full",
    "pg_stat_get_wal_bytes",
    "pg_stat_get_wal_fpi",
    "pg_stat_get_wal_records",
    "pg_stat_get_wal_sync",
    "pg_stat_get_wal_sync_time",
    "pg_stat_get_wal_write",
    "pg_stat_get_wal_write_time",
    "pg_stop_backup",
    "pg_terminate_backend_with_timeout",
    "pg_wait_for_backend_termination",
    "quote",
    "rand",
    "row",
    "row_count",
    "sleep",
    "spg_build_time",
    "spg_edition",
    "spg_uptime_seconds",
    "spg_version",
    "unix_timestamp",
    "user",
    "uuid_generate_v7",
    "uuid_short",
    "xmlforest",
];

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
    // 9.0.0 (S1) — the three-argument forms. They were left out on the
    // reasoning that the engine could not answer them, and `SELECT
    // lag(1, 1, 1) OVER ()` did refuse — but for the FROM clause, not
    // the arity: a window function with no FROM was refused outright.
    // With that closed, both answer exactly what PostgreSQL 18.6
    // answers (`lag(5,1,99) OVER (ORDER BY x)` over two rows is
    // `99, 5`), so the missing rows were the catalog understating what
    // the engine does.
    (3108, "lag", "w", 3, 5077),
    (3109, "lead", "w", 1, 2283),
    (3110, "lead", "w", 2, 2283),
    (3111, "lead", "w", 3, 5077),
    (3112, "first_value", "w", 1, 2283),
    (3113, "last_value", "w", 1, 2283),
    (3114, "nth_value", "w", 2, 2283),
    // v7.39 (round 638, F20) — the functions SPG implements that this table
    // did not list. It had 90 rows against PG's 3415, so a tool asking
    // "does this function exist" was told no for almost everything the
    // engine can do.
    //
    // Every row is measured, not declared. The dispatcher's names were
    // probed against the engine (rounds 636/637), the arity read out of its
    // own "takes N arg" errors, and the return type from
    // `pg_typeof(f(NULL...))` with typed NULLs where an untyped one left it
    // unknown. Only the 283 whose return type could actually be measured
    // are here; the rest are stubs whose result is an untyped NULL, and
    // listing them with a made-up type would be inventing the thing this
    // catalog exists to report.
    //
    // `pronargs` comes from PG where PG has the function — that is the
    // signature the implementation targets, and SPG's stubs accept any
    // arity, so its own arity probe is not evidence of the intended one.
    // For the 86 functions PG does not have, the measured arity is all
    // there is, and their oids are synthesised above 900000.
    (1601, "acos", "f", 1, 701),
    (338, "amvalidate", "f", 1, 16),
    (1620, "ascii", "f", 1, 23),
    (1600, "asin", "f", 1, 701),
    (1602, "atan", "f", 1, 701),
    (1603, "atan2", "f", 2, 701),
    (900001, "benchmark", "f", 0, 23),
    (1811, "bit_length", "f", 1, 23),
    (3952, "brin_summarize_new_values", "f", 1, 20),
    (1372, "char_length", "f", 1, 23),
    (1367, "character_length", "f", 1, 23),
    (900002, "connection_id", "f", 0, 20),
    (1605, "cos", "f", 1, 701),
    (900003, "current_catalog", "f", 0, 25),
    (817, "current_query", "f", 0, 25),
    (900004, "current_role", "f", 0, 25),
    (1403, "current_schemas", "f", 1, 1009),
    (900005, "database", "f", 0, 25),
    (6221, "date_add", "f", 2, 1184),
    (6177, "date_bin", "f", 3, 1114),
    (6223, "date_subtract", "f", 2, 1184),
    (4292, "datemultirange", "f", 0, 4535),
    (3941, "daterange", "f", 2, 3912),
    (1608, "degrees", "f", 1, 701),
    (900006, "field", "f", 2, 23),
    (900007, "found_rows", "f", 0, 20),
    (900008, "from_unixtime", "f", 1, 1114),
    (900009, "gen_uuid_v7", "f", 0, 2950),
    (3759, "get_current_ts_config", "f", 0, 25),
    (1039, "getdatabaseencoding", "f", 0, 25),
    (3789, "gin_clean_pending_list", "f", 1, 20),
    (3724, "gin_cmp_tslexeme", "f", 2, 23),
    (3480, "gin_compare_jsonb", "f", 2, 23),
    (3029, "has_any_column_privilege", "f", 2, 16),
    (3021, "has_column_privilege", "f", 3, 16),
    (2255, "has_database_privilege", "f", 2, 16),
    (2261, "has_function_privilege", "f", 2, 16),
    (2272, "has_schema_privilege", "f", 2, 16),
    (2185, "has_sequence_privilege", "f", 2, 16),
    (6099, "icu_unicode_version", "f", 0, 25),
    (900010, "ifnull", "f", 2, 23),
    (4280, "int4multirange", "f", 0, 4451),
    (3840, "int4range", "f", 2, 3904),
    (4295, "int8multirange", "f", 0, 4536),
    (3945, "int8range", "f", 2, 3926),
    (900087, "isnull", "f", 1, 23),
    (900011, "json_array", "f", 0, 114),
    (900012, "last_insert_id", "f", 0, 20),
    (1741, "log", "f", 1, 701),
    (900013, "log2", "f", 1, 701),
    (3846, "make_date", "f", 3, 1082),
    (3464, "make_interval", "f", 7, 1186),
    (3461, "make_timestamp", "f", 6, 1114),
    (3462, "make_timestamptz", "f", 6, 1184),
    (1728, "mod", "f", 2, 23),
    (900014, "nullif", "f", 2, 23),
    (440, "num_nonnulls", "f", 1, 23),
    (438, "num_nulls", "f", 1, 23),
    (4283, "nummultirange", "f", 0, 4532),
    (3844, "numrange", "f", 2, 3906),
    (720, "octet_length", "f", 1, 23),
    (2884, "pg_advisory_unlock", "f", 1, 16),
    (2885, "pg_advisory_unlock_shared", "f", 1, 16),
    (2026, "pg_backend_pid", "f", 0, 23),
    (900015, "pg_backend_start_time", "f", 0, 1114),
    (2172, "pg_backup_start", "f", 2, 25),
    (2739, "pg_backup_stop", "f", 1, 25),
    (810, "pg_client_encoding", "f", 0, 25),
    (3448, "pg_collation_actual_version", "f", 1, 25),
    (1269, "pg_column_size", "f", 1, 23),
    (2034, "pg_conf_load_time", "f", 0, 1114),
    (3098, "pg_create_restore_point", "f", 1, 25),
    (900016, "pg_current_edition", "f", 0, 25),
    (3800, "pg_current_logfile", "f", 0, 25),
    (900017, "pg_current_query", "f", 0, 25),
    (5061, "pg_current_snapshot", "f", 0, 25),
    (3330, "pg_current_wal_flush_lsn", "f", 0, 25),
    (2852, "pg_current_wal_insert_lsn", "f", 0, 25),
    (2849, "pg_current_wal_lsn", "f", 0, 25),
    (5059, "pg_current_xact_id", "f", 0, 20),
    (6249, "pg_database_collation_actual_version", "f", 1, 25),
    (2324, "pg_database_size", "f", 1, 20),
    (900018, "pg_get_wait_event_name", "f", 0, 25),
    (900019, "pg_get_wait_event_type", "f", 0, 25),
    (1137, "pg_get_wal_replay_pause_state", "f", 0, 25),
    (2710, "pg_has_role", "f", 2, 16),
    (3445, "pg_import_system_collations", "f", 1, 23),
    (638, "pg_index_column_has_property", "f", 3, 16),
    (637, "pg_index_has_property", "f", 2, 16),
    (636, "pg_indexam_has_property", "f", 2, 16),
    (900020, "pg_is_in_backup", "f", 0, 16),
    (3810, "pg_is_in_recovery", "f", 0, 16),
    (3073, "pg_is_wal_replay_paused", "f", 0, 16),
    (3378, "pg_isolation_test_session_is_blocked", "f", 2, 16),
    (315, "pg_jit_available", "f", 0, 16),
    (3820, "pg_last_wal_receive_lsn", "f", 0, 25),
    (3821, "pg_last_wal_replay_lsn", "f", 0, 25),
    (900021, "pg_last_xid", "f", 0, 20),
    (3577, "pg_logical_emit_message", "f", 4, 25),
    (3296, "pg_notification_queue_usage", "f", 0, 701),
    (900022, "pg_object_size", "f", 0, 20),
    (2560, "pg_postmaster_start_time", "f", 0, 1114),
    (900023, "pg_prewarm", "f", 0, 20),
    (3436, "pg_promote", "f", 2, 16),
    (3034, "pg_relation_filepath", "f", 1, 25),
    (6121, "pg_relation_is_publishable", "f", 1, 16),
    (900024, "pg_relation_size_pretty", "f", 0, 20),
    (2621, "pg_reload_conf", "f", 0, 16),
    (2622, "pg_rotate_logfile", "f", 0, 16),
    (900025, "pg_rotate_logfile_v2", "f", 0, 16),
    (900026, "pg_start_backup", "f", 0, 25),
    (3056, "pg_stat_get_analyze_count", "f", 1, 20),
    (900027, "pg_stat_get_archiver_archived_count", "f", 0, 20),
    (900028, "pg_stat_get_archiver_failed_count", "f", 0, 20),
    (900029, "pg_stat_get_archiver_last_archived_wal", "f", 0, 25),
    (900030, "pg_stat_get_archiver_last_failed_wal", "f", 0, 25),
    (3057, "pg_stat_get_autoanalyze_count", "f", 1, 20),
    (3055, "pg_stat_get_autovacuum_count", "f", 1, 20),
    (1391, "pg_stat_get_backend_start", "f", 1, 1114),
    (
        900031,
        "pg_stat_get_bgwriter_buf_written_checkpoints",
        "f",
        0,
        20,
    ),
    (2772, "pg_stat_get_bgwriter_buf_written_clean", "f", 0, 20),
    (2773, "pg_stat_get_bgwriter_maxwritten_clean", "f", 0, 20),
    (
        900032,
        "pg_stat_get_bgwriter_requested_checkpoints",
        "f",
        0,
        20,
    ),
    (900033, "pg_stat_get_bgwriter_timed_checkpoints", "f", 0, 20),
    (1934, "pg_stat_get_blocks_fetched", "f", 1, 20),
    (1935, "pg_stat_get_blocks_hit", "f", 1, 20),
    (2859, "pg_stat_get_buf_alloc", "f", 0, 20),
    (900034, "pg_stat_get_buf_fsync_backend", "f", 0, 20),
    (900035, "pg_stat_get_buf_written_backend", "f", 0, 20),
    (900036, "pg_stat_get_checkpoint_sync_time", "f", 0, 20),
    (900037, "pg_stat_get_checkpoint_write_time", "f", 0, 20),
    (2771, "pg_stat_get_checkpointer_buffers_written", "f", 0, 20),
    (2770, "pg_stat_get_checkpointer_num_requested", "f", 0, 20),
    (2769, "pg_stat_get_checkpointer_num_timed", "f", 0, 20),
    (
        6329,
        "pg_stat_get_checkpointer_restartpoints_performed",
        "f",
        0,
        20,
    ),
    (
        6328,
        "pg_stat_get_checkpointer_restartpoints_requested",
        "f",
        0,
        20,
    ),
    (
        6327,
        "pg_stat_get_checkpointer_restartpoints_timed",
        "f",
        0,
        20,
    ),
    (6314, "pg_stat_get_checkpointer_stat_reset_time", "f", 0, 20),
    (3161, "pg_stat_get_checkpointer_sync_time", "f", 0, 20),
    (3160, "pg_stat_get_checkpointer_write_time", "f", 0, 20),
    (6186, "pg_stat_get_db_active_time", "f", 1, 20),
    (2844, "pg_stat_get_db_blk_read_time", "f", 1, 20),
    (2845, "pg_stat_get_db_blk_write_time", "f", 1, 20),
    (1944, "pg_stat_get_db_blocks_fetched", "f", 1, 20),
    (1945, "pg_stat_get_db_blocks_hit", "f", 1, 20),
    (3426, "pg_stat_get_db_checksum_failures", "f", 1, 20),
    (3070, "pg_stat_get_db_conflict_all", "f", 1, 20),
    (3068, "pg_stat_get_db_conflict_bufferpin", "f", 1, 20),
    (3066, "pg_stat_get_db_conflict_lock", "f", 1, 20),
    (6309, "pg_stat_get_db_conflict_logicalslot", "f", 1, 20),
    (3067, "pg_stat_get_db_conflict_snapshot", "f", 1, 20),
    (3069, "pg_stat_get_db_conflict_startup_deadlock", "f", 1, 20),
    (3065, "pg_stat_get_db_conflict_tablespace", "f", 1, 20),
    (3152, "pg_stat_get_db_deadlocks", "f", 1, 20),
    (6187, "pg_stat_get_db_idle_in_transaction_time", "f", 1, 20),
    (1941, "pg_stat_get_db_numbackends", "f", 1, 20),
    (6185, "pg_stat_get_db_session_time", "f", 1, 20),
    (6188, "pg_stat_get_db_sessions", "f", 1, 20),
    (6189, "pg_stat_get_db_sessions_abandoned", "f", 1, 20),
    (6190, "pg_stat_get_db_sessions_fatal", "f", 1, 20),
    (6191, "pg_stat_get_db_sessions_killed", "f", 1, 20),
    (3151, "pg_stat_get_db_temp_bytes", "f", 1, 20),
    (3150, "pg_stat_get_db_temp_files", "f", 1, 20),
    (2762, "pg_stat_get_db_tuples_deleted", "f", 1, 20),
    (2759, "pg_stat_get_db_tuples_fetched", "f", 1, 20),
    (2760, "pg_stat_get_db_tuples_inserted", "f", 1, 20),
    (2758, "pg_stat_get_db_tuples_returned", "f", 1, 20),
    (2761, "pg_stat_get_db_tuples_updated", "f", 1, 20),
    (1942, "pg_stat_get_db_xact_commit", "f", 1, 20),
    (1943, "pg_stat_get_db_xact_rollback", "f", 1, 20),
    (2879, "pg_stat_get_dead_tuples", "f", 1, 20),
    (2978, "pg_stat_get_function_calls", "f", 1, 20),
    (2980, "pg_stat_get_function_self_time", "f", 1, 20),
    (2979, "pg_stat_get_function_total_time", "f", 1, 20),
    (900038, "pg_stat_get_idx_scan", "f", 0, 20),
    (900039, "pg_stat_get_idx_tup_fetch", "f", 0, 20),
    (900040, "pg_stat_get_idx_tup_read", "f", 0, 20),
    (5053, "pg_stat_get_ins_since_vacuum", "f", 1, 20),
    (2878, "pg_stat_get_live_tuples", "f", 1, 20),
    (3177, "pg_stat_get_mod_since_analyze", "f", 1, 20),
    (1928, "pg_stat_get_numscans", "f", 1, 20),
    (6248, "pg_stat_get_recovery_prefetch", "f", 0, 20),
    (
        900041,
        "pg_stat_get_recovery_prefetch_reset_time",
        "f",
        0,
        20,
    ),
    (900042, "pg_stat_get_seq_scan", "f", 0, 20),
    (900043, "pg_stat_get_seq_scan_pos", "f", 0, 20),
    (900044, "pg_stat_get_seq_tup_read", "f", 0, 20),
    (900045, "pg_stat_get_slru_blks_exists", "f", 0, 20),
    (900046, "pg_stat_get_slru_blks_hit", "f", 0, 20),
    (900047, "pg_stat_get_slru_blks_read", "f", 0, 20),
    (900048, "pg_stat_get_slru_blks_written", "f", 0, 20),
    (900049, "pg_stat_get_slru_blks_zeroed", "f", 0, 20),
    (900050, "pg_stat_get_slru_flushes", "f", 0, 20),
    (900051, "pg_stat_get_slru_stat_reset_time", "f", 0, 20),
    (900052, "pg_stat_get_slru_truncates", "f", 0, 20),
    (3788, "pg_stat_get_snapshot_timestamp", "f", 0, 1114),
    (900053, "pg_stat_get_stat_snapshot_timestamp", "f", 0, 1114),
    (900054, "pg_stat_get_tid_scan_pos", "f", 0, 20),
    (1933, "pg_stat_get_tuples_deleted", "f", 1, 20),
    (1930, "pg_stat_get_tuples_fetched", "f", 1, 20),
    (1972, "pg_stat_get_tuples_hot_updated", "f", 1, 20),
    (1931, "pg_stat_get_tuples_inserted", "f", 1, 20),
    (6217, "pg_stat_get_tuples_newpage_updated", "f", 1, 20),
    (1929, "pg_stat_get_tuples_returned", "f", 1, 20),
    (1932, "pg_stat_get_tuples_updated", "f", 1, 20),
    (3054, "pg_stat_get_vacuum_count", "f", 1, 20),
    (900055, "pg_stat_get_wal_buffers_full", "f", 0, 20),
    (900056, "pg_stat_get_wal_bytes", "f", 0, 20),
    (900057, "pg_stat_get_wal_fpi", "f", 0, 20),
    (900058, "pg_stat_get_wal_records", "f", 0, 20),
    (900059, "pg_stat_get_wal_sync", "f", 0, 20),
    (900060, "pg_stat_get_wal_sync_time", "f", 0, 20),
    (900061, "pg_stat_get_wal_write", "f", 0, 20),
    (900062, "pg_stat_get_wal_write_time", "f", 0, 20),
    (3044, "pg_stat_get_xact_blocks_fetched", "f", 1, 20),
    (3045, "pg_stat_get_xact_blocks_hit", "f", 1, 20),
    (3046, "pg_stat_get_xact_function_calls", "f", 1, 20),
    (3048, "pg_stat_get_xact_function_self_time", "f", 1, 20),
    (3047, "pg_stat_get_xact_function_total_time", "f", 1, 20),
    (3037, "pg_stat_get_xact_numscans", "f", 1, 20),
    (3042, "pg_stat_get_xact_tuples_deleted", "f", 1, 20),
    (3039, "pg_stat_get_xact_tuples_fetched", "f", 1, 20),
    (3043, "pg_stat_get_xact_tuples_hot_updated", "f", 1, 20),
    (3040, "pg_stat_get_xact_tuples_inserted", "f", 1, 20),
    (6218, "pg_stat_get_xact_tuples_newpage_updated", "f", 1, 20),
    (3038, "pg_stat_get_xact_tuples_returned", "f", 1, 20),
    (3041, "pg_stat_get_xact_tuples_updated", "f", 1, 20),
    (900063, "pg_stop_backup", "f", 0, 25),
    (3778, "pg_tablespace_location", "f", 1, 25),
    (900064, "pg_terminate_backend_with_timeout", "f", 0, 16),
    (3163, "pg_trigger_depth", "f", 0, 23),
    (2882, "pg_try_advisory_lock", "f", 1, 16),
    (2883, "pg_try_advisory_lock_shared", "f", 1, 16),
    (3091, "pg_try_advisory_xact_lock", "f", 1, 16),
    (3092, "pg_try_advisory_xact_lock_shared", "f", 1, 16),
    (900065, "pg_wait_for_backend_termination", "f", 0, 16),
    (1610, "pi", "f", 0, 701),
    (849, "position", "f", 2, 23),
    (1738, "pow", "f", 2, 23),
    (900066, "quote", "f", 1, 25),
    (1289, "quote_nullable", "f", 1, 25),
    (1609, "radians", "f", 1, 701),
    (900067, "rand", "f", 0, 701),
    (6212, "random_normal", "f", 2, 701),
    (900068, "row", "f", 0, 2249),
    (900069, "row_count", "f", 0, 20),
    (1706, "sign", "f", 1, 23),
    // 9.0.0 — pg_trgm returns `real` (700), measured on 18.6. It said
    // float8.
    (900070, "similarity", "f", 2, 700),
    (900086, "show_trgm", "f", 1, 1009),
    (900087, "word_similarity", "f", 2, 700),
    (900088, "strict_word_similarity", "f", 2, 700),
    (900089, "set_limit", "f", 1, 700),
    (900090, "show_limit", "f", 0, 700),
    (900091, "similarity_op", "f", 2, 16),
    (900092, "similarity_dist", "f", 2, 700),
    (900093, "word_similarity_op", "f", 2, 16),
    (900094, "word_similarity_commutator_op", "f", 2, 16),
    (900095, "word_similarity_dist_op", "f", 2, 700),
    (900096, "word_similarity_dist_commutator_op", "f", 2, 700),
    (900097, "strict_word_similarity_op", "f", 2, 16),
    (900098, "strict_word_similarity_commutator_op", "f", 2, 16),
    (900099, "strict_word_similarity_dist_op", "f", 2, 700),
    (
        900100,
        "strict_word_similarity_dist_commutator_op",
        "f",
        2,
        700,
    ),
    (1604, "sin", "f", 1, 701),
    (900071, "sleep", "f", 0, 23),
    (900072, "spg_build_time", "f", 0, 25),
    (900073, "spg_edition", "f", 0, 25),
    (900074, "spg_uptime_seconds", "f", 0, 20),
    (900075, "spg_version", "f", 0, 25),
    (3696, "starts_with", "f", 2, 16),
    (868, "strpos", "f", 2, 23),
    (6311, "system_user", "f", 0, 25),
    (1606, "tan", "f", 1, 701),
    (274, "timeofday", "f", 0, 25),
    (1159, "timezone", "f", 2, 1184),
    (1780, "to_date", "f", 2, 1082),
    (1158, "to_timestamp", "f", 1, 1184),
    (4112, "trunc", "f", 1, 23),
    (4286, "tsmultirange", "f", 0, 4533),
    (3933, "tsrange", "f", 2, 3908),
    (4289, "tstzmultirange", "f", 0, 4534),
    (3937, "tstzrange", "f", 2, 3910),
    (2943, "txid_current", "f", 0, 20),
    (2944, "txid_current_snapshot", "f", 0, 25),
    (4549, "unicode_version", "f", 0, 25),
    (900076, "unix_timestamp", "f", 0, 20),
    (900077, "user", "f", 0, 25),
    (6342, "uuid_extract_timestamp", "f", 1, 1184),
    (900101, "uuid_generate_v1", "f", 0, 2950),
    (900102, "uuid_generate_v1mc", "f", 0, 2950),
    (900103, "uuid_generate_v3", "f", 2, 2950),
    (900078, "uuid_generate_v4", "f", 0, 2950),
    (900104, "uuid_generate_v5", "f", 2, 2950),
    (900079, "uuid_generate_v7", "f", 0, 2950),
    (900080, "uuid_nil", "f", 0, 2950),
    (900081, "uuid_ns_dns", "f", 0, 2950),
    (900082, "uuid_ns_oid", "f", 0, 2950),
    (900083, "uuid_ns_url", "f", 0, 2950),
    (900084, "uuid_ns_x500", "f", 0, 2950),
    (900085, "uuid_short", "f", 0, 20),
    (6429, "uuidv7", "f", 0, 2950),
    (89, "version", "f", 0, 25),
    (900086, "xmlforest", "f", 0, 142),
    (3050, "xpath_exists", "f", 2, 16),
    // v7.39 (round 653) — F20. `pg_proc` listed 338 function names while the
    // engine answered 709: SPG was under-reporting its own surface by more
    // than half, so every client that introspects pg_proc — psql's \df, ORMs
    // probing for a function before using it, migration tools — was told SPG
    // lacks functions it implements.
    //
    // The 235 names below were established by MEASUREMENT, not by reading the
    // dispatch table: each candidate was called with zero arguments and the
    // engine's own reply separates "does not exist" from "takes N args", so
    // the list is what the running binary really answers, and the arity comes
    // out of the same reply. Each row then takes PG18's real
    // (oid, prokind, pronargs, prorettype) for that name and arity.
    //
    // What is deliberately NOT here, for the same reason the pg_operator
    // synthesis lists only real operators: 149 further names the engine
    // answers that PG18 does not have — the MySQL-dialect family
    // (`date_format`, `ucase`, `year`, …), the extension families
    // (pgcrypto's `crypt`/`armor`, fuzzystrmatch's `levenshtein`, amcheck's
    // `verify_heapam`), and SPG's own internals (`__array_assign`,
    // `spg_injection_*`). Listing those in pg_proc would claim PG has them.
    // `interval` was dropped too: it parses as a cast keyword, not a function.
    (13298, "_pg_char_max_length", "f", 2, 23),
    (13299, "_pg_char_octet_length", "f", 2, 23),
    (13304, "_pg_datetime_precision", "f", 2, 23),
    (13301, "_pg_numeric_precision", "f", 2, 23),
    (13303, "_pg_numeric_scale", "f", 2, 23),
    (598, "abbrev", "f", 1, 25),
    (599, "abbrev", "f", 1, 25),
    (3943, "acldefault", "f", 2, 1034),
    (2732, "acosd", "f", 1, 701),
    (2466, "acosh", "f", 1, 701),
    (378, "array_append", "f", 2, 5078),
    (383, "array_cat", "f", 2, 5078),
    (747, "array_dims", "f", 1, 25),
    (1193, "array_fill", "f", 2, 2277),
    (1286, "array_fill", "f", 3, 2277),
    (2176, "array_length", "f", 2, 23),
    (2091, "array_lower", "f", 2, 23),
    (748, "array_ndims", "f", 1, 23),
    (3277, "array_position", "f", 2, 23),
    (3278, "array_position", "f", 3, 23),
    (3279, "array_positions", "f", 2, 1007),
    (379, "array_prepend", "f", 2, 5078),
    (3167, "array_remove", "f", 2, 5078),
    (3168, "array_replace", "f", 3, 5078),
    (6381, "array_reverse", "f", 1, 2277),
    (6216, "array_sample", "f", 2, 2277),
    (6215, "array_shuffle", "f", 1, 2277),
    (6388, "array_sort", "f", 1, 2277),
    (6389, "array_sort", "f", 2, 2277),
    (6390, "array_sort", "f", 3, 2277),
    (3153, "array_to_json", "f", 1, 114),
    (3154, "array_to_json", "f", 2, 114),
    (395, "array_to_string", "f", 2, 25),
    (384, "array_to_string", "f", 3, 25),
    (3327, "array_to_tsvector", "f", 1, 3614),
    (2092, "array_upper", "f", 2, 23),
    (2731, "asind", "f", 1, 701),
    (2465, "asinh", "f", 1, 701),
    (2734, "atan2d", "f", 2, 701),
    (2733, "atand", "f", 1, 701),
    (2467, "atanh", "f", 1, 701),
    (6163, "bit_count", "f", 1, 20),
    (6162, "bit_count", "f", 1, 20),
    (698, "broadcast", "f", 1, 869),
    (2011, "byteacat", "f", 2, 17),
    (3179, "cardinality", "f", 1, 23),
    (6412, "casefold", "f", 1, 25),
    (935, "cash_words", "f", 1, 25),
    (1345, "cbrt", "f", 1, 701),
    (1621, "chr", "f", 1, 25),
    (1813, "convert", "f", 3, 17),
    (1714, "convert_from", "f", 2, 25),
    (1717, "convert_to", "f", 2, 17),
    (2736, "cosd", "f", 1, 701),
    (2463, "cosh", "f", 1, 701),
    (1607, "cot", "f", 1, 701),
    (2738, "cotd", "f", 1, 701),
    (6364, "crc32", "f", 1, 20),
    (6365, "crc32c", "f", 1, 20),
    (2077, "current_setting", "f", 1, 25),
    (3294, "current_setting", "f", 2, 25),
    (1575, "currval", "f", 1, 20),
    (1947, "decode", "f", 2, 17),
    (1973, "div", "f", 2, 1700),
    (1946, "encode", "f", 2, 25),
    (6219, "erf", "f", 1, 701),
    (6220, "erfc", "f", 1, 701),
    (1376, "factorial", "f", 1, 1700),
    (711, "family", "f", 1, 23),
    (5044, "gcd", "f", 2, 23),
    (5045, "gcd", "f", 2, 20),
    (5048, "gcd", "f", 2, 1700),
    (1192, "generate_subscripts", "f", 2, 23),
    (1191, "generate_subscripts", "f", 3, 23),
    (723, "get_bit", "f", 2, 23),
    (3032, "get_bit", "f", 2, 23),
    (721, "get_byte", "f", 2, 23),
    (1926, "has_table_privilege", "f", 2, 16),
    (1927, "has_table_privilege", "f", 2, 16),
    (1923, "has_table_privilege", "f", 3, 16),
    (1922, "has_table_privilege", "f", 3, 16),
    (1925, "has_table_privilege", "f", 3, 16),
    (1924, "has_table_privilege", "f", 3, 16),
    (6413, "hashbytea", "f", 1, 23),
    (449, "hashint2", "f", 1, 23),
    (450, "hashint4", "f", 1, 23),
    (949, "hashint8", "f", 1, 23),
    (400, "hashtext", "f", 1, 23),
    (699, "host", "f", 1, 25),
    (4063, "inet_merge", "f", 2, 650),
    (4071, "inet_same_family", "f", 2, 16),
    (872, "initcap", "f", 1, 25),
    (4351, "is_normalized", "f", 2, 16),
    (4237, "isempty", "f", 1, 16),
    (3850, "isempty", "f", 1, 16),
    (2048, "isfinite", "f", 1, 16),
    (1373, "isfinite", "f", 1, 16),
    (1390, "isfinite", "f", 1, 16),
    (1389, "isfinite", "f", 1, 16),
    (3956, "json_array_length", "f", 1, 23),
    (3202, "json_object", "f", 1, 114),
    (3203, "json_object", "f", 2, 114),
    (3957, "json_object_keys", "f", 1, 25),
    (3261, "json_strip_nulls", "f", 2, 114),
    (4215, "json_to_tsvector", "f", 2, 3614),
    (4216, "json_to_tsvector", "f", 3, 3614),
    (3968, "json_typeof", "f", 1, 25),
    (3207, "jsonb_array_length", "f", 1, 23),
    (3301, "jsonb_concat", "f", 2, 3802),
    (4050, "jsonb_contained", "f", 2, 16),
    (4046, "jsonb_contains", "f", 2, 16),
    (3343, "jsonb_delete", "f", 2, 3802),
    (3302, "jsonb_delete", "f", 2, 3802),
    (3303, "jsonb_delete", "f", 2, 3802),
    (3304, "jsonb_delete_path", "f", 2, 3802),
    (4047, "jsonb_exists", "f", 2, 16),
    (4049, "jsonb_exists_all", "f", 2, 16),
    (4048, "jsonb_exists_any", "f", 2, 16),
    (3579, "jsonb_insert", "f", 4, 3802),
    (3263, "jsonb_object", "f", 1, 3802),
    (3264, "jsonb_object", "f", 2, 3802),
    (3931, "jsonb_object_keys", "f", 1, 25),
    (4005, "jsonb_path_exists", "f", 4, 16),
    (4009, "jsonb_path_match", "f", 4, 16),
    (4006, "jsonb_path_query", "f", 4, 3802),
    (4007, "jsonb_path_query_array", "f", 4, 3802),
    (4008, "jsonb_path_query_first", "f", 4, 3802),
    (3306, "jsonb_pretty", "f", 1, 25),
    (3305, "jsonb_set", "f", 4, 3802),
    (5054, "jsonb_set_lax", "f", 5, 3802),
    (3262, "jsonb_strip_nulls", "f", 2, 3802),
    (4213, "jsonb_to_tsvector", "f", 2, 3614),
    (4214, "jsonb_to_tsvector", "f", 3, 3614),
    (3210, "jsonb_typeof", "f", 1, 25),
    (1295, "justify_days", "f", 1, 1186),
    (1175, "justify_hours", "f", 1, 1186),
    (2711, "justify_interval", "f", 1, 1186),
    (2559, "lastval", "f", 0, 20),
    (5047, "lcm", "f", 2, 20),
    (5049, "lcm", "f", 2, 1700),
    (5046, "lcm", "f", 2, 23),
    (3060, "left", "f", 2, 25),
    (1637, "like_escape", "f", 2, 25),
    (2009, "like_escape", "f", 2, 17),
    (1481, "log10", "f", 1, 1700),
    (1194, "log10", "f", 1, 701),
    (3851, "lower_inc", "f", 1, 16),
    (4238, "lower_inc", "f", 1, 16),
    (3853, "lower_inf", "f", 1, 16),
    (4240, "lower_inf", "f", 1, 16),
    (879, "lpad", "f", 2, 25),
    (873, "lpad", "f", 3, 25),
    (4125, "macaddr8_set7bit", "f", 1, 774),
    (3847, "make_time", "f", 3, 1083),
    (1365, "makeaclitem", "f", 4, 1033),
    (697, "masklen", "f", 1, 23),
    (2321, "md5", "f", 1, 25),
    (2311, "md5", "f", 1, 25),
    (5042, "min_scale", "f", 1, 23),
    (4298, "multirange", "f", 1, 4537),
    (696, "netmask", "f", 1, 869),
    (683, "network", "f", 1, 650),
    (1574, "nextval", "f", 1, 20),
    (4350, "normalize", "f", 2, 25),
    (3672, "numnode", "f", 1, 23),
    (1348, "obj_description", "f", 1, 25),
    (1215, "obj_description", "f", 2, 25),
    (1405, "overlay", "f", 3, 25),
    (752, "overlay", "f", 3, 17),
    (3031, "overlay", "f", 3, 1560),
    (1404, "overlay", "f", 4, 25),
    (749, "overlay", "f", 4, 17),
    (3030, "overlay", "f", 4, 1560),
    (1268, "parse_ident", "f", 2, 1009),
    (6315, "pg_basetype", "f", 1, 2206),
    (3162, "pg_collation_for", "f", 1, 25),
    (2121, "pg_column_compression", "f", 1, 25),
    (2319, "pg_encoding_max_length", "f", 1, 23),
    (4568, "pg_event_trigger_ddl_commands", "f", 0, 2249),
    (3566, "pg_event_trigger_dropped_objects", "f", 0, 2249),
    (4566, "pg_event_trigger_table_rewrite_oid", "f", 0, 26),
    (4567, "pg_event_trigger_table_rewrite_reason", "f", 0, 23),
    (1665, "pg_get_serial_sequence", "f", 2, 25),
    (6210, "pg_input_is_valid", "f", 2, 16),
    (3252, "pg_lsn_hash", "f", 1, 23),
    (4187, "pg_lsn_larger", "f", 2, 3220),
    (4188, "pg_lsn_smaller", "f", 2, 3220),
    (3425, "pg_partition_ancestors", "f", 1, 2205),
    (3424, "pg_partition_root", "f", 1, 2205),
    (3334, "pg_size_bytes", "f", 1, 20),
    (3166, "pg_size_pretty", "f", 1, 25),
    (2288, "pg_size_pretty", "f", 1, 25),
    (3165, "pg_wal_lsn_diff", "f", 2, 1700),
    (5066, "pg_xact_status", "f", 1, 25),
    (5001, "phraseto_tsquery", "f", 1, 3615),
    (5006, "phraseto_tsquery", "f", 2, 3615),
    (3751, "plainto_tsquery", "f", 1, 3615),
    (3747, "plainto_tsquery", "f", 2, 3615),
    (1440, "point", "f", 2, 600),
    (3673, "querytree", "f", 1, 25),
    (3862, "range_adjacent", "f", 2, 16),
    (4057, "range_merge", "f", 2, 3831),
    (6254, "regexp_count", "f", 2, 23),
    (6255, "regexp_count", "f", 3, 23),
    (6256, "regexp_count", "f", 4, 23),
    (6257, "regexp_instr", "f", 2, 23),
    (6258, "regexp_instr", "f", 3, 23),
    (6259, "regexp_instr", "f", 4, 23),
    (6260, "regexp_instr", "f", 5, 23),
    (6261, "regexp_instr", "f", 6, 23),
    (6262, "regexp_instr", "f", 7, 23),
    (6263, "regexp_like", "f", 2, 16),
    (6264, "regexp_like", "f", 3, 16),
    (3396, "regexp_match", "f", 2, 1009),
    (3397, "regexp_match", "f", 3, 1009),
    (2763, "regexp_matches", "f", 2, 1009),
    (2764, "regexp_matches", "f", 3, 1009),
    (2284, "regexp_replace", "f", 3, 25),
    (2285, "regexp_replace", "f", 4, 25),
    (6253, "regexp_replace", "f", 4, 25),
    (6252, "regexp_replace", "f", 5, 25),
    (6251, "regexp_replace", "f", 6, 25),
    (2767, "regexp_split_to_array", "f", 2, 1009),
    (2768, "regexp_split_to_array", "f", 3, 1009),
    (6265, "regexp_substr", "f", 2, 25),
    (6266, "regexp_substr", "f", 3, 25),
    (6267, "regexp_substr", "f", 4, 25),
    (6268, "regexp_substr", "f", 5, 25),
    (1622, "repeat", "f", 2, 25),
    (2087, "replace", "f", 3, 25),
    (3062, "reverse", "f", 1, 25),
    (6382, "reverse", "f", 1, 17),
    (3061, "right", "f", 2, 25),
    (3155, "row_to_json", "f", 1, 114),
    (880, "rpad", "f", 2, 25),
    (874, "rpad", "f", 3, 25),
    (3281, "scale", "f", 1, 23),
    (724, "set_bit", "f", 3, 17),
    (3033, "set_bit", "f", 3, 1560),
    (722, "set_byte", "f", 3, 17),
    (605, "set_masklen", "f", 2, 869),
    (635, "set_masklen", "f", 2, 650),
    (1599, "setseed", "f", 1, 2278),
    (1576, "setval", "f", 2, 20),
    (1765, "setval", "f", 3, 20),
    (3624, "setweight", "f", 2, 3614),
    (3320, "setweight", "f", 3, 3614),
    (3419, "sha224", "f", 1, 17),
    (3420, "sha256", "f", 1, 17),
    (3421, "sha384", "f", 1, 17),
    (3422, "sha512", "f", 1, 17),
    (1623, "similar_escape", "f", 2, 25),
    (1987, "similar_to_escape", "f", 1, 25),
    (1986, "similar_to_escape", "f", 2, 25),
    (2735, "sind", "f", 1, 701),
    (2462, "sinh", "f", 1, 701),
    (2088, "split_part", "f", 3, 25),
    (394, "string_to_array", "f", 2, 1009),
    (376, "string_to_array", "f", 3, 1009),
    (3623, "strip", "f", 1, 3614),
    (2086, "substr", "f", 2, 17),
    (883, "substr", "f", 2, 25),
    (877, "substr", "f", 3, 25),
    (2085, "substr", "f", 3, 17),
    (1291, "suppress_redundant_updates_trigger", "f", 0, 2279),
    (2737, "tand", "f", 1, 701),
    (2464, "tanh", "f", 1, 701),
    (743, "text_ge", "f", 2, 16),
    (742, "text_gt", "f", 2, 16),
    (741, "text_le", "f", 2, 16),
    (740, "text_lt", "f", 2, 16),
    (1258, "textcat", "f", 2, 25),
    (67, "texteq", "f", 2, 16),
    (157, "textne", "f", 2, 16),
    (1845, "to_ascii", "f", 1, 25),
    (1846, "to_ascii", "f", 2, 25),
    (1847, "to_ascii", "f", 2, 25),
    (6330, "to_bin", "f", 1, 25),
    (6331, "to_bin", "f", 1, 25),
    (2089, "to_hex", "f", 1, 25),
    (2090, "to_hex", "f", 1, 25),
    (3176, "to_json", "f", 1, 114),
    (3787, "to_jsonb", "f", 1, 3802),
    (1777, "to_number", "f", 2, 1700),
    (6332, "to_oct", "f", 1, 25),
    (6333, "to_oct", "f", 1, 25),
    (3750, "to_tsquery", "f", 1, 3615),
    (3746, "to_tsquery", "f", 2, 3615),
    (3749, "to_tsvector", "f", 1, 3614),
    (4209, "to_tsvector", "f", 1, 3614),
    (4210, "to_tsvector", "f", 1, 3614),
    (4211, "to_tsvector", "f", 2, 3614),
    (4212, "to_tsvector", "f", 2, 3614),
    (3745, "to_tsvector", "f", 2, 3614),
    (878, "translate", "f", 3, 25),
    (6172, "trim_array", "f", 2, 2277),
    (5043, "trim_scale", "f", 1, 1700),
    (3323, "ts_delete", "f", 2, 3614),
    (3321, "ts_delete", "f", 2, 3614),
    (3319, "ts_filter", "f", 2, 3614),
    (3755, "ts_headline", "f", 2, 25),
    (4204, "ts_headline", "f", 2, 3802),
    (4208, "ts_headline", "f", 2, 114),
    (3754, "ts_headline", "f", 3, 25),
    (4207, "ts_headline", "f", 3, 114),
    (4206, "ts_headline", "f", 3, 114),
    (4203, "ts_headline", "f", 3, 3802),
    (4202, "ts_headline", "f", 3, 3802),
    (3744, "ts_headline", "f", 3, 25),
    (4201, "ts_headline", "f", 4, 3802),
    (4205, "ts_headline", "f", 4, 114),
    (3743, "ts_headline", "f", 4, 25),
    (3723, "ts_lexize", "f", 2, 1009),
    (3706, "ts_rank", "f", 2, 700),
    (3705, "ts_rank", "f", 3, 700),
    (3704, "ts_rank", "f", 3, 700),
    (3703, "ts_rank", "f", 4, 700),
    (3710, "ts_rank_cd", "f", 2, 700),
    (3708, "ts_rank_cd", "f", 3, 700),
    (3709, "ts_rank_cd", "f", 3, 700),
    (3707, "ts_rank_cd", "f", 4, 700),
    (3684, "ts_rewrite", "f", 3, 3615),
    (3669, "tsquery_and", "f", 2, 3615),
    (3671, "tsquery_not", "f", 1, 3615),
    (3670, "tsquery_or", "f", 2, 3615),
    (5003, "tsquery_phrase", "f", 2, 3615),
    (5004, "tsquery_phrase", "f", 3, 3615),
    (3326, "tsvector_to_array", "f", 1, 1009),
    (3752, "tsvector_update_trigger", "f", 0, 2279),
    (3753, "tsvector_update_trigger_column", "f", 0, 2279),
    (3360, "txid_status", "f", 1, 25),
    (6198, "unistr", "f", 1, 25),
    (4239, "upper_inc", "f", 1, 16),
    (3852, "upper_inc", "f", 1, 16),
    (4241, "upper_inf", "f", 1, 16),
    (3854, "upper_inf", "f", 1, 16),
    (6343, "uuid_extract_version", "f", 1, 21),
    (5009, "websearch_to_tsquery", "f", 1, 3615),
    (5007, "websearch_to_tsquery", "f", 2, 3615),
    (2170, "width_bucket", "f", 4, 23),
    (320, "width_bucket", "f", 4, 23),
    (2895, "xmlcomment", "f", 1, 142),
    (3813, "xmltext", "f", 1, 142),
    // v7.39 (round 654) — F20's second layer: the OVERLOADS. Round 653 fixed
    // which function NAMES are listed; 81 of those names still carried fewer
    // rows than PG18 (213 rows short, `max`/`min` at 3 against 24).
    //
    // The design question had to be settled before any row went in, because
    // SPG's `max` is ONE generic implementation, not 24 typed ones — so is
    // listing 24 rows a description of "how many real overloads exist" (in
    // which case it is fiction) or of "which calls succeed" (in which case
    // it is true)? Measured: `max` really does answer for smallint, bigint,
    // real, numeric, money, interval, date, time, timestamp, timestamptz,
    // text, char, bytea and inet, and `sum`/`avg`/`abs`/`mod` likewise. The
    // catalog describes what a client may call, so the rows are true.
    //
    // Each row below was earned the same way: a call was CONSTRUCTED for
    // that exact signature from PG18's `pg_get_function_arguments`, sent to
    // the engine, and kept only if the engine answered. Roughly twenty of
    // PG's overloads were refused — `timezone(interval, …)`, three-argument
    // `lag`/`lead`, three-argument `date_add`/`date_subtract` — and those
    // are NOT listed: they are capability gaps, recorded as such, not
    // catalog gaps to paper over.
    (1394, "abs", "f", 1, 700),
    (1395, "abs", "f", 1, 701),
    (1398, "abs", "f", 1, 21),
    (1386, "age", "f", 1, 1186),
    (1199, "age", "f", 2, 1186),
    (4053, "array_agg", "a", 1, 2277),
    (2101, "avg", "a", 1, 1700),
    (2102, "avg", "a", 1, 1700),
    (2103, "avg", "a", 1, 1700),
    (2104, "avg", "a", 1, 701),
    (2105, "avg", "a", 1, 701),
    (2106, "avg", "a", 1, 1186),
    (1810, "bit_length", "f", 1, 23),
    (1812, "bit_length", "f", 1, 23),
    (2015, "btrim", "f", 2, 17),
    (1381, "char_length", "f", 1, 23),
    (1369, "character_length", "f", 1, 23),
    (6178, "date_bin", "f", 3, 1184),
    (1171, "date_part", "f", 2, 701),
    (1172, "date_part", "f", 2, 701),
    (1384, "date_part", "f", 2, 701),
    (1217, "date_trunc", "f", 2, 1184),
    (1218, "date_trunc", "f", 2, 1186),
    (4293, "datemultirange", "f", 1, 4535),
    (3025, "has_any_column_privilege", "f", 3, 16),
    (3027, "has_any_column_privilege", "f", 3, 16),
    (3022, "has_column_privilege", "f", 3, 16),
    (3023, "has_column_privilege", "f", 3, 16),
    (3014, "has_column_privilege", "f", 4, 16),
    (3015, "has_column_privilege", "f", 4, 16),
    (3018, "has_column_privilege", "f", 4, 16),
    (3019, "has_column_privilege", "f", 4, 16),
    (2254, "has_database_privilege", "f", 2, 16),
    (2250, "has_database_privilege", "f", 3, 16),
    (2251, "has_database_privilege", "f", 3, 16),
    (2252, "has_database_privilege", "f", 3, 16),
    (2253, "has_database_privilege", "f", 3, 16),
    (2257, "has_function_privilege", "f", 3, 16),
    (2259, "has_function_privilege", "f", 3, 16),
    (2273, "has_schema_privilege", "f", 2, 16),
    (2268, "has_schema_privilege", "f", 3, 16),
    (2269, "has_schema_privilege", "f", 3, 16),
    (2270, "has_schema_privilege", "f", 3, 16),
    (2271, "has_schema_privilege", "f", 3, 16),
    (2186, "has_sequence_privilege", "f", 2, 16),
    (2182, "has_sequence_privilege", "f", 3, 16),
    (2184, "has_sequence_privilege", "f", 3, 16),
    (4281, "int4multirange", "f", 1, 4451),
    (4296, "int8multirange", "f", 1, 4536),
    (3199, "json_build_array", "f", 0, 114),
    (3201, "json_build_object", "f", 0, 114),
    (3272, "jsonb_build_array", "f", 0, 3802),
    (3274, "jsonb_build_object", "f", 0, 3802),
    (1340, "log", "f", 1, 701),
    (6195, "ltrim", "f", 2, 17),
    (2050, "max", "a", 1, 2277),
    (2115, "max", "a", 1, 20),
    (2117, "max", "a", 1, 21),
    (2118, "max", "a", 1, 26),
    (2119, "max", "a", 1, 700),
    (2120, "max", "a", 1, 701),
    (2122, "max", "a", 1, 1082),
    (2123, "max", "a", 1, 1083),
    (2124, "max", "a", 1, 1266),
    (2125, "max", "a", 1, 790),
    (2126, "max", "a", 1, 1114),
    (2127, "max", "a", 1, 1184),
    (2128, "max", "a", 1, 1186),
    (2244, "max", "a", 1, 1042),
    (2797, "max", "a", 1, 27),
    (3564, "max", "a", 1, 869),
    (4189, "max", "a", 1, 3220),
    (5099, "max", "a", 1, 5069),
    (6395, "max", "a", 1, 17),
    (2051, "min", "a", 1, 2277),
    (2131, "min", "a", 1, 20),
    (2133, "min", "a", 1, 21),
    (2134, "min", "a", 1, 26),
    (2135, "min", "a", 1, 700),
    (2136, "min", "a", 1, 701),
    (2138, "min", "a", 1, 1082),
    (2139, "min", "a", 1, 1083),
    (2140, "min", "a", 1, 1266),
    (2141, "min", "a", 1, 790),
    (2142, "min", "a", 1, 1114),
    (2143, "min", "a", 1, 1184),
    (2144, "min", "a", 1, 1186),
    (2245, "min", "a", 1, 1042),
    (2798, "min", "a", 1, 27),
    (3565, "min", "a", 1, 869),
    (4190, "min", "a", 1, 3220),
    (5100, "min", "a", 1, 5069),
    (6396, "min", "a", 1, 17),
    (940, "mod", "f", 2, 21),
    (941, "mod", "f", 2, 23),
    (947, "mod", "f", 2, 20),
    (4284, "nummultirange", "f", 1, 4532),
    (1374, "octet_length", "f", 1, 23),
    (1375, "octet_length", "f", 1, 23),
    (1682, "octet_length", "f", 1, 23),
    (2890, "pg_advisory_unlock", "f", 2, 16),
    (2891, "pg_advisory_unlock_shared", "f", 2, 16),
    (3801, "pg_current_logfile", "f", 1, 25),
    (2168, "pg_database_size", "f", 1, 20),
    (2709, "pg_has_role", "f", 2, 16),
    (2705, "pg_has_role", "f", 3, 16),
    (2706, "pg_has_role", "f", 3, 16),
    (2707, "pg_has_role", "f", 3, 16),
    (2708, "pg_has_role", "f", 3, 16),
    (3578, "pg_logical_emit_message", "f", 4, 3220),
    (2888, "pg_try_advisory_lock", "f", 2, 16),
    (2889, "pg_try_advisory_lock_shared", "f", 2, 16),
    (3095, "pg_try_advisory_xact_lock", "f", 2, 16),
    (3096, "pg_try_advisory_xact_lock_shared", "f", 2, 16),
    (2014, "position", "f", 2, 23),
    (1346, "pow", "f", 2, 701),
    (1290, "quote_nullable", "f", 1, 25),
    (6339, "random", "f", 2, 23),
    (6340, "random", "f", 2, 20),
    (6341, "random", "f", 2, 1700),
    (6196, "rtrim", "f", 2, 17),
    (2310, "sign", "f", 1, 701),
    (1699, "substring", "f", 2, 1560),
    (2013, "substring", "f", 2, 17),
    (2073, "substring", "f", 2, 25),
    (1680, "substring", "f", 3, 1560),
    (2012, "substring", "f", 3, 17),
    (2074, "substring", "f", 3, 25),
    (2107, "sum", "a", 1, 1700),
    (2109, "sum", "a", 1, 20),
    (2110, "sum", "a", 1, 700),
    (2111, "sum", "a", 1, 701),
    (2112, "sum", "a", 1, 790),
    (2113, "sum", "a", 1, 1186),
    (2037, "timezone", "f", 2, 1266),
    (2069, "timezone", "f", 2, 1184),
    (1768, "to_char", "f", 2, 25),
    (1770, "to_char", "f", 2, 25),
    (1773, "to_char", "f", 2, 25),
    (1774, "to_char", "f", 2, 25),
    (1776, "to_char", "f", 2, 25),
    (1778, "to_timestamp", "f", 2, 1184),
    (753, "trunc", "f", 1, 829),
    (1343, "trunc", "f", 1, 701),
    (1710, "trunc", "f", 1, 1700),
    (1709, "trunc", "f", 2, 1700),
    (4287, "tsmultirange", "f", 1, 4533),
    (4290, "tstzmultirange", "f", 1, 4534),
    (6430, "uuidv7", "f", 1, 2950),
    (3218, "width_bucket", "f", 2, 23),
    // v7.39 (round 663) — F20's tail. Round 654 left 63 rows across 43 names
    // looking like capability gaps; re-measured, most were the PROBE's fault
    // again — range constructors want `'[]'` for the flags argument and got
    // `'a'`, privilege functions want a real relation and got `'SELECT'`.
    // Called properly, `point(box)`, three-argument range constructors,
    // `int4multirange()`, the hypothetical-set aggregates, three-argument
    // `date_trunc` and one-argument `age` all answer; they were listed
    // nowhere. Same rule as round 654: a call was constructed for the exact
    // PG18 signature and the row kept only if the engine answered.
    (3942, "daterange", "f", 3, 3912),
    (3841, "int4range", "f", 3, 3904),
    (3946, "int8range", "f", 3, 3926),
    (4235, "lower", "f", 1, 2283),
    (3463, "make_timestamptz", "f", 7, 1184),
    (6373, "max", "a", 1, 2249),
    (6374, "min", "a", 1, 2249),
    (3845, "numrange", "f", 3, 3906),
    (1534, "point", "f", 1, 600),
    (4228, "range_merge", "f", 1, 3831),
    (3156, "row_to_json", "f", 2, 114),
    (1775, "to_char", "f", 2, 25),
    (3934, "tsrange", "f", 3, 3908),
    (3938, "tstzrange", "f", 3, 3910),
    (4236, "upper", "f", 1, 2283),
    // 9.0.0 — the I/O functions every type names in `typinput` /
    // `typoutput` / `typreceive` / `typsend`, and the handler every
    // access method names in `amhandler`. Those columns read `-`
    // (oid 0, PostgreSQL's spelling for "no function"), because SPG's
    // I/O is built into the engine and had no catalogued function to
    // point at — so `pg_type.typinput` said `-` where PG 18.6 says
    // `int4in`, and a join to `pg_proc` had nothing to find either
    // way. Oids, names, kinds, arities and return types are PG 18.6's,
    // read off it; none collides with the 876 rows above.
    (3, "heap_tableam_handler", "f", 1, 269),
    (267, "table_am_handler_in", "f", 1, 269),
    (268, "table_am_handler_out", "f", 1, 2275),
    (326, "index_am_handler_in", "f", 1, 325),
    (327, "index_am_handler_out", "f", 1, 2275),
    (2292, "cstring_in", "f", 1, 2275),
    (2293, "cstring_out", "f", 1, 2275),
    (2500, "cstring_recv", "f", 1, 2275),
    (2501, "cstring_send", "f", 1, 17),
    (31, "byteaout", "f", 1, 2275),
    (33, "charout", "f", 1, 2275),
    (34, "namein", "f", 1, 19),
    (35, "nameout", "f", 1, 2275),
    (38, "int2in", "f", 1, 21),
    (39, "int2out", "f", 1, 2275),
    (40, "int2vectorin", "f", 1, 22),
    (41, "int2vectorout", "f", 1, 2275),
    (42, "int4in", "f", 1, 23),
    (43, "int4out", "f", 1, 2275),
    (44, "regprocin", "f", 1, 24),
    (45, "regprocout", "f", 1, 2275),
    (46, "textin", "f", 1, 25),
    (47, "textout", "f", 1, 2275),
    (48, "tidin", "f", 1, 27),
    (49, "tidout", "f", 1, 2275),
    (50, "xidin", "f", 1, 28),
    (51, "xidout", "f", 1, 2275),
    (52, "cidin", "f", 1, 29),
    (53, "cidout", "f", 1, 2275),
    (54, "oidvectorin", "f", 1, 30),
    (55, "oidvectorout", "f", 1, 2275),
    (117, "point_in", "f", 1, 600),
    (118, "point_out", "f", 1, 2275),
    (195, "pg_node_tree_in", "f", 1, 194),
    (196, "pg_node_tree_out", "f", 1, 2275),
    (197, "pg_node_tree_recv", "f", 1, 194),
    (198, "pg_node_tree_send", "f", 1, 17),
    (200, "float4in", "f", 1, 700),
    (201, "float4out", "f", 1, 2275),
    (214, "float8in", "f", 1, 701),
    (215, "float8out", "f", 1, 2275),
    (321, "json_in", "f", 1, 114),
    (322, "json_out", "f", 1, 2275),
    (323, "json_recv", "f", 1, 114),
    (324, "json_send", "f", 1, 17),
    (330, "bthandler", "f", 1, 325),
    (331, "hashhandler", "f", 1, 325),
    (332, "gisthandler", "f", 1, 325),
    (333, "ginhandler", "f", 1, 325),
    (334, "spghandler", "f", 1, 325),
    (335, "brinhandler", "f", 1, 325),
    (436, "macaddr_in", "f", 1, 829),
    (437, "macaddr_out", "f", 1, 2275),
    (460, "int8in", "f", 1, 20),
    (461, "int8out", "f", 1, 2275),
    (750, "array_in", "f", 3, 2277),
    (751, "array_out", "f", 1, 2275),
    (886, "cash_in", "f", 1, 790),
    (887, "cash_out", "f", 1, 2275),
    (910, "inet_in", "f", 1, 869),
    (911, "inet_out", "f", 1, 2275),
    (1031, "aclitemin", "f", 1, 1033),
    (1032, "aclitemout", "f", 1, 2275),
    (1044, "bpcharin", "f", 3, 1042),
    (1045, "bpcharout", "f", 1, 2275),
    (1046, "varcharin", "f", 3, 1043),
    (1047, "varcharout", "f", 1, 2275),
    (1084, "date_in", "f", 1, 1082),
    (1085, "date_out", "f", 1, 2275),
    (1143, "time_in", "f", 3, 1083),
    (1144, "time_out", "f", 1, 2275),
    (1150, "timestamptz_in", "f", 3, 1184),
    (1151, "timestamptz_out", "f", 1, 2275),
    (1160, "interval_in", "f", 3, 1186),
    (1161, "interval_out", "f", 1, 2275),
    (1242, "boolin", "f", 1, 16),
    (1243, "boolout", "f", 1, 2275),
    (1244, "byteain", "f", 1, 17),
    (1245, "charin", "f", 1, 18),
    (1267, "cidr_in", "f", 1, 650),
    (1312, "timestamp_in", "f", 3, 1114),
    (1313, "timestamp_out", "f", 1, 2275),
    (1350, "timetz_in", "f", 3, 1266),
    (1351, "timetz_out", "f", 1, 2275),
    (1427, "cidr_out", "f", 1, 2275),
    (1564, "bit_in", "f", 3, 1560),
    (1565, "bit_out", "f", 1, 2275),
    (1579, "varbit_in", "f", 3, 1562),
    (1580, "varbit_out", "f", 1, 2275),
    (1701, "numeric_in", "f", 3, 1700),
    (1702, "numeric_out", "f", 1, 2275),
    (1798, "oidin", "f", 1, 26),
    (1799, "oidout", "f", 1, 2275),
    (2218, "regclassin", "f", 1, 2205),
    (2219, "regclassout", "f", 1, 2275),
    (2220, "regtypein", "f", 1, 2206),
    (2221, "regtypeout", "f", 1, 2275),
    (2290, "record_in", "f", 3, 2249),
    (2291, "record_out", "f", 1, 2275),
    (2296, "anyarray_in", "f", 1, 2277),
    (2297, "anyarray_out", "f", 1, 2275),
    (2298, "void_in", "f", 1, 2278),
    (2299, "void_out", "f", 1, 2275),
    (2300, "trigger_in", "f", 1, 2279),
    (2301, "trigger_out", "f", 1, 2275),
    (2312, "anyelement_in", "f", 1, 2283),
    (2313, "anyelement_out", "f", 1, 2275),
    (2400, "array_recv", "f", 3, 2277),
    (2401, "array_send", "f", 1, 17),
    (2402, "record_recv", "f", 3, 2249),
    (2403, "record_send", "f", 1, 17),
    (2404, "int2recv", "f", 1, 21),
    (2405, "int2send", "f", 1, 17),
    (2406, "int4recv", "f", 1, 23),
    (2407, "int4send", "f", 1, 17),
    (2408, "int8recv", "f", 1, 20),
    (2409, "int8send", "f", 1, 17),
    (2410, "int2vectorrecv", "f", 1, 22),
    (2411, "int2vectorsend", "f", 1, 17),
    (2412, "bytearecv", "f", 1, 17),
    (2413, "byteasend", "f", 1, 17),
    (2414, "textrecv", "f", 1, 25),
    (2415, "textsend", "f", 1, 17),
    (2418, "oidrecv", "f", 1, 26),
    (2419, "oidsend", "f", 1, 17),
    (2420, "oidvectorrecv", "f", 1, 30),
    (2421, "oidvectorsend", "f", 1, 17),
    (2422, "namerecv", "f", 1, 19),
    (2423, "namesend", "f", 1, 17),
    (2424, "float4recv", "f", 1, 700),
    (2425, "float4send", "f", 1, 17),
    (2426, "float8recv", "f", 1, 701),
    (2427, "float8send", "f", 1, 17),
    (2428, "point_recv", "f", 1, 600),
    (2429, "point_send", "f", 1, 17),
    (2430, "bpcharrecv", "f", 3, 1042),
    (2431, "bpcharsend", "f", 1, 17),
    (2432, "varcharrecv", "f", 3, 1043),
    (2433, "varcharsend", "f", 1, 17),
    (2434, "charrecv", "f", 1, 18),
    (2435, "charsend", "f", 1, 17),
    (2436, "boolrecv", "f", 1, 16),
    (2437, "boolsend", "f", 1, 17),
    (2438, "tidrecv", "f", 1, 27),
    (2439, "tidsend", "f", 1, 17),
    (2440, "xidrecv", "f", 1, 28),
    (2441, "xidsend", "f", 1, 17),
    (2442, "cidrecv", "f", 1, 29),
    (2443, "cidsend", "f", 1, 17),
    (2444, "regprocrecv", "f", 1, 24),
    (2445, "regprocsend", "f", 1, 17),
    (2452, "regclassrecv", "f", 1, 2205),
    (2453, "regclasssend", "f", 1, 17),
    (2454, "regtyperecv", "f", 1, 2206),
    (2455, "regtypesend", "f", 1, 17),
    (2456, "bit_recv", "f", 3, 1560),
    (2457, "bit_send", "f", 1, 17),
    (2458, "varbit_recv", "f", 3, 1562),
    (2459, "varbit_send", "f", 1, 17),
    (2460, "numeric_recv", "f", 3, 1700),
    (2461, "numeric_send", "f", 1, 17),
    (2468, "date_recv", "f", 1, 1082),
    (2469, "date_send", "f", 1, 17),
    (2470, "time_recv", "f", 3, 1083),
    (2471, "time_send", "f", 1, 17),
    (2472, "timetz_recv", "f", 3, 1266),
    (2473, "timetz_send", "f", 1, 17),
    (2474, "timestamp_recv", "f", 3, 1114),
    (2475, "timestamp_send", "f", 1, 17),
    (2476, "timestamptz_recv", "f", 3, 1184),
    (2477, "timestamptz_send", "f", 1, 17),
    (2478, "interval_recv", "f", 3, 1186),
    (2479, "interval_send", "f", 1, 17),
    (2492, "cash_recv", "f", 1, 790),
    (2493, "cash_send", "f", 1, 17),
    (2494, "macaddr_recv", "f", 1, 829),
    (2495, "macaddr_send", "f", 1, 17),
    (2496, "inet_recv", "f", 1, 869),
    (2497, "inet_send", "f", 1, 17),
    (2498, "cidr_recv", "f", 1, 650),
    (2499, "cidr_send", "f", 1, 17),
    (2502, "anyarray_recv", "f", 1, 2277),
    (2503, "anyarray_send", "f", 1, 17),
    (2893, "xml_in", "f", 1, 142),
    (2894, "xml_out", "f", 1, 2275),
    (2898, "xml_recv", "f", 1, 142),
    (2899, "xml_send", "f", 1, 17),
    (2952, "uuid_in", "f", 1, 2950),
    (2953, "uuid_out", "f", 1, 2275),
    (2961, "uuid_recv", "f", 1, 2950),
    (2962, "uuid_send", "f", 1, 17),
    (3120, "void_recv", "f", 1, 2278),
    (3121, "void_send", "f", 1, 17),
    (3229, "pg_lsn_in", "f", 1, 3220),
    (3230, "pg_lsn_out", "f", 1, 2275),
    (3238, "pg_lsn_recv", "f", 1, 3220),
    (3239, "pg_lsn_send", "f", 1, 17),
    (3446, "macaddr8_recv", "f", 1, 774),
    (3447, "macaddr8_send", "f", 1, 17),
    (3610, "tsvectorin", "f", 1, 3614),
    (3611, "tsvectorout", "f", 1, 2275),
    (3612, "tsqueryin", "f", 1, 3615),
    (3613, "tsqueryout", "f", 1, 2275),
    (3638, "tsvectorsend", "f", 1, 17),
    (3639, "tsvectorrecv", "f", 1, 3614),
    (3640, "tsquerysend", "f", 1, 17),
    (3641, "tsqueryrecv", "f", 1, 3615),
    (3803, "jsonb_send", "f", 1, 17),
    (3804, "jsonb_out", "f", 1, 2275),
    (3805, "jsonb_recv", "f", 1, 3802),
    (3806, "jsonb_in", "f", 1, 3802),
    (3832, "anyrange_in", "f", 3, 3831),
    (3833, "anyrange_out", "f", 1, 2275),
    (3834, "range_in", "f", 3, 3831),
    (3835, "range_out", "f", 1, 2275),
    (3836, "range_recv", "f", 3, 3831),
    (3837, "range_send", "f", 1, 17),
    (4110, "macaddr8_in", "f", 1, 774),
    (4111, "macaddr8_out", "f", 1, 2275),
    (4229, "anymultirange_in", "f", 3, 4537),
    (4230, "anymultirange_out", "f", 1, 2275),
    (4231, "multirange_in", "f", 3, 4537),
    (4232, "multirange_out", "f", 1, 2275),
    (4233, "multirange_recv", "f", 3, 4537),
    (4234, "multirange_send", "f", 1, 17),
    (5070, "xid8in", "f", 1, 5069),
    (5081, "xid8out", "f", 1, 2275),
    (5082, "xid8recv", "f", 1, 5069),
    (5083, "xid8send", "f", 1, 17),
    (5088, "anycompatiblearray_in", "f", 1, 5078),
    (5089, "anycompatiblearray_out", "f", 1, 2275),
    (5090, "anycompatiblearray_recv", "f", 1, 5078),
    (5091, "anycompatiblearray_send", "f", 1, 17),
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
    for (name, _) in engine.effective_users().iter() {
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
    for tname in cat.visible_relation_keys() {
        // 9.0.0 (C9) — the loop walks KEYS; `tname` is the name a client
        // reads and `nsp` the schema column it belongs in, while every
        // catalog lookup below goes on `key`.
        let key = tname;
        let nsp = alloc::string::String::from(cat.listed_schema(&key));
        let Some(tname) = cat.listed_name(&key).map(alloc::string::String::from) else {
            continue;
        };
        let Some(t) = cat.get(&key) else { continue };
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
    for tname in cat.visible_relation_keys() {
        // 9.0.0 (C9) — the loop walks KEYS; `tname` is the name a client
        // reads and `nsp` the schema column it belongs in, while every
        // catalog lookup below goes on `key`.
        let key = tname;
        let nsp = alloc::string::String::from(cat.listed_schema(&key));
        let Some(tname) = cat.listed_name(&key).map(alloc::string::String::from) else {
            continue;
        };
        let Some(t) = cat.get(&key) else { continue };
        // Same PG-canonical `{table}_{col}_check` naming pg_constraint
        // and pg_get_constraintdef use, so the three agree.
        let check_names = pg_check_connames(t, &tname, &t.schema().checks);
        for (ci, clause) in t.schema().checks.iter().enumerate() {
            rows.push(Row::new(alloc::vec![
                Value::text("spg"),
                Value::text(nsp.clone()),
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
                Value::text(nsp.clone()),
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
    for (name, def) in crate::sequence::catalog_sequences(cat) {
        let Some(name) = cat.listed_name(&name) else {
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

/// v7.39.11 — the column list this view serves, which is the session's
/// dialect's own.
///
/// Reported by sentori against 7.39.10, the mirror of the defect
/// 7.39.2 closed for `SHOW CREATE TABLE`: through that version every
/// session got MySQL's shape, so a PostgreSQL session had no
/// `table_schema` and `WHERE table_schema = 'public'` — the way every
/// tool scopes this view — raised `column "table_schema" does not
/// exist`, while `referenced_table_name` and `referenced_column_name`,
/// which PostgreSQL does not have, were there instead.
///
/// Measured on PostgreSQL 18.6 and MySQL 9.7.2: the two lists agree on
/// the first nine, in that order, and MySQL adds three. So the rows are
/// built once, in MySQL's order, and a PostgreSQL session is served the
/// first nine.
const KCU_PG_COLUMNS: usize = 9;

pub(crate) fn synth_info_key_column_usage(
    cat: &Catalog,
    mysql: bool,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let mut schema = alloc::vec![
        ColumnSchema::new("constraint_catalog", DataType::Text, false),
        ColumnSchema::new("constraint_schema", DataType::Text, false),
        ColumnSchema::new("constraint_name", DataType::Text, false),
        ColumnSchema::new("table_catalog", DataType::Text, false),
        ColumnSchema::new("table_schema", DataType::Text, false),
        ColumnSchema::new("table_name", DataType::Text, false),
        ColumnSchema::new("column_name", DataType::Text, false),
        ColumnSchema::new("ordinal_position", DataType::Int, false),
        // v7.39 (round 266) — where this column sits in the parent key
        // the FK points at. NULL on PK/UNIQUE rows, which is how a
        // reflection tool tells an FK row apart from a key row.
        ColumnSchema::new("position_in_unique_constraint", DataType::Int, true),
        // MySQL's three, and only MySQL's.
        ColumnSchema::new("referenced_table_schema", DataType::Text, true),
        ColumnSchema::new("referenced_table_name", DataType::Text, true),
        ColumnSchema::new("referenced_column_name", DataType::Text, true),
    ];
    if !mysql {
        schema.truncate(KCU_PG_COLUMNS);
    }
    let mut rows: Vec<Row<'static>> = Vec::new();
    for tname in cat.visible_relation_keys() {
        // 9.0.0 (C9) — the loop walks KEYS; `tname` is the name a client
        // reads and `nsp` the schema column it belongs in, while every
        // catalog lookup below goes on `key`.
        let key = tname;
        let nsp = alloc::string::String::from(cat.listed_schema(&key));
        let Some(tname) = cat.listed_name(&key).map(alloc::string::String::from) else {
            continue;
        };
        let Some(t) = cat.get(&key) else { continue };
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
                let mut vals = alloc::vec![
                    Value::text("spg"),
                    Value::text(nsp.clone()),
                    Value::text(conname.clone()),
                    Value::text("spg"),
                    Value::text(nsp.clone()),
                    Value::text(tname.clone()),
                    Value::text(col_name_at(local)),
                    Value::Int(ordinal),
                    Value::Int(in_unique),
                    Value::text(alloc::string::String::from(spg_sql::namespace::schema_of(
                        &fk.parent_table
                    ))),
                    Value::text(fk.parent_table.clone()),
                    Value::text(parent_name),
                ];
                if !mysql {
                    vals.truncate(KCU_PG_COLUMNS);
                }
                rows.push(Row::new(vals));
            }
        }
        // PK / composite UC entries.
        for uc in t.schema().uniqueness_constraints.iter() {
            let conname = pg_unique_conname(t, uc, &tname);
            for (i, &local) in uc.columns.iter().enumerate() {
                #[allow(clippy::cast_possible_wrap)]
                let ordinal = (i + 1) as i32;
                let mut vals = alloc::vec![
                    Value::text("spg"),
                    Value::text(nsp.clone()),
                    Value::text(conname.clone()),
                    Value::text("spg"),
                    Value::text(nsp.clone()),
                    Value::text(tname.clone()),
                    Value::text(col_name_at(local)),
                    Value::Int(ordinal),
                    // A key row is not a foreign key, and PG leaves
                    // this NULL rather than 0.
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                ];
                if !mysql {
                    vals.truncate(KCU_PG_COLUMNS);
                }
                rows.push(Row::new(vals));
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
    for tname in cat.visible_relation_keys() {
        // 9.0.0 (C9) — the loop walks KEYS; `tname` is the name a client
        // reads and `nsp` the schema column it belongs in, while every
        // catalog lookup below goes on `key`.
        let key = tname;
        let nsp = alloc::string::String::from(cat.listed_schema(&key));
        let Some(tname) = cat.listed_name(&key).map(alloc::string::String::from) else {
            continue;
        };
        let Some(t) = cat.get(&key) else { continue };
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
            let qualifier = |text: alloc::string::String| {
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
                Value::text(nsp.clone()),
                qualifier(alloc::string::String::from("spg")),
                qualifier(alloc::string::String::from(spg_sql::namespace::schema_of(
                    &fk.parent_table
                ))),
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
        // v7.40.0 — MySQL's index prefix, `KEY k (b(4))`. NULL for a
        // whole-column key, which is every PostgreSQL one.
        ColumnSchema::new("sub_part", DataType::Int, true),
        ColumnSchema::new("index_type", DataType::Text, false),
    ];
    // v7.40.0 — built from [`catalog_indexes`], the same table the
    // PostgreSQL surface reads.
    //
    // It used to walk `t.indices()` directly, and so answered three
    // different wrong things about the same table at once. Measured
    // against MySQL 9.7.2, `PRIMARY KEY (a, b)`:
    //
    // ```text
    //                       MySQL 9.7.2          SPG 7.39.13
    //   index_name          PRIMARY (both rows)  mc_i_a_pkey_0_0
    //                                            mc_i_b_pkey_0_1
    //   seq_in_index        1, 2                 1, 1
    //   non_unique          0                    1
    // ```
    //
    // A composite key reached the reader as two unrelated
    // single-column indexes, each claiming not to be unique — the
    // v7.39.10 defect (`SHOW INDEX` said the primary key was not
    // unique) and the v7.39.12 one (composite keys missing on 20 of 27
    // tables), both still open here because this surface had its own
    // copy of the walk. One table now.
    let mut rows: Vec<Row<'static>> = Vec::new();
    for idx in catalog_indexes(cat) {
        let Some(t) = cat.get(&idx.table) else {
            continue;
        };
        for (seq, pos) in idx.columns.iter().enumerate() {
            let col = t
                .schema()
                .columns
                .get(*pos)
                .map_or("?".into(), |c| c.name.clone());
            rows.push(Row::new(alloc::vec![
                Value::text(idx.table.clone()),
                Value::text(mysql_index_name(&idx)),
                Value::text(col),
                Value::Int(i32::try_from(seq + 1).unwrap_or(1)),
                Value::Int(i32::from(!idx.is_unique)),
                t.indices()
                    .iter()
                    .find(|i| i.name == idx.name)
                    .and_then(|i| i.prefix_len)
                    .filter(|_| seq == 0)
                    .map_or(Value::Null, |p| {
                        Value::Int(i32::try_from(p).unwrap_or(0))
                    }),
                Value::text("BTREE"),
            ]));
        }
    }
    (schema, rows)
}

/// MySQL names every primary key `PRIMARY`, whatever the constraint was
/// called. Anything else keeps the name it was declared with.
pub(crate) fn mysql_index_name(idx: &CatalogIndex) -> alloc::string::String {
    if idx.is_primary {
        alloc::string::String::from("PRIMARY")
    } else {
        idx.name.clone()
    }
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
/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows. The rows and the
/// columns come from one place, so `pg_attribute` cannot describe
/// a relation differently from how it answers.
pub(crate) fn pg_sequence_schema() -> Vec<ColumnSchema> {
    alloc::vec![
        ColumnSchema::new("seqrelid", DataType::BigInt, false),
        ColumnSchema::new("seqtypid", DataType::BigInt, false),
        ColumnSchema::new("seqstart", DataType::BigInt, false),
        ColumnSchema::new("seqincrement", DataType::BigInt, false),
        ColumnSchema::new("seqmax", DataType::BigInt, false),
        ColumnSchema::new("seqmin", DataType::BigInt, false),
        ColumnSchema::new("seqcache", DataType::BigInt, false),
        ColumnSchema::new("seqcycle", DataType::Bool, false),
    ]
}

pub(crate) fn synth_pg_sequence(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_sequence_schema();
    let mut rows: Vec<Row<'static>> = Vec::new();
    // v7.39 (round 338, V64) — seqrelid IS the sequence's pg_class oid, so
    // it reads from the shared allocator. It used to number sequences from
    // 32768 while pg_class numbered them from 300_000, which broke PG's
    // canonical `pg_class JOIN pg_sequence ON oid = seqrelid` outright —
    // and 32768 was the view band, so the two kinds collided besides.
    for (name, def) in crate::sequence::catalog_sequences(cat) {
        let Some(name) = cat.listed_name(&name) else {
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
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("conname", DataType::Name, false),
        ColumnSchema::new("connamespace", DataType::Oid, false),
        ColumnSchema::new("contype", DataType::Char1, false),
        ColumnSchema::new("condeferrable", DataType::Bool, false),
        ColumnSchema::new("condeferred", DataType::Bool, false),
        // v7.39 (round 543) — PG18's NOT ENFORCED support. SPG enforces
        // every constraint it accepts, so this is always true.
        ColumnSchema::new("conenforced", DataType::Bool, false),
        ColumnSchema::new("convalidated", DataType::Bool, false),
        ColumnSchema::new("conrelid", DataType::Oid, false),
        ColumnSchema::new("contypid", DataType::Oid, false),
        ColumnSchema::new("conindid", DataType::Oid, false),
        ColumnSchema::new("conparentid", DataType::Oid, false),
        ColumnSchema::new("confrelid", DataType::Oid, false),
        ColumnSchema::new("confupdtype", DataType::Char1, false),
        ColumnSchema::new("confdeltype", DataType::Char1, false),
        ColumnSchema::new("confmatchtype", DataType::Char1, false),
        ColumnSchema::new("conislocal", DataType::Bool, false),
        ColumnSchema::new("coninhcount", DataType::SmallInt, false),
        ColumnSchema::new("connoinherit", DataType::Bool, false),
        // v7.39 (round 543) — WITHOUT OVERLAPS; SPG has no temporal
        // constraints.
        ColumnSchema::new("conperiod", DataType::Bool, false),
        ColumnSchema::new("conkey", DataType::SmallIntArray, false),
        ColumnSchema::new("confkey", DataType::SmallIntArray, true),
        // The three operator arrays a foreign key records, the ON DELETE
        // SET column list and the exclusion operators — NULL here; SPG
        // has no pg_operator to name oids from.
        ColumnSchema::new("conpfeqop", DataType::OidArray, true),
        ColumnSchema::new("conppeqop", DataType::OidArray, true),
        ColumnSchema::new("conffeqop", DataType::OidArray, true),
        ColumnSchema::new("confdelsetcols", DataType::SmallIntArray, true),
        ColumnSchema::new("conexclop", DataType::OidArray, true),
        // conbin is what a tool tests to know a constraint is a CHECK.
        // Measured on PG18: non-NULL for contype 'c', NULL for every
        // other kind. SPG keeps the written expression, the same choice
        // pg_attrdef.adbin already makes here.
        ColumnSchema::new("conbin", DataType::PgNodeTree, true),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    let constraint_index: alloc::collections::BTreeMap<(String, usize), i64> = catalog_indexes(cat)
        .into_iter()
        .filter_map(|ix| Some(((ix.table, ix.backs_constraint?), ix.oid)))
        .collect();
    // Build the same name → oid map pg_class uses (start at 16384).
    let names = cat.visible_relation_keys();
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
        // 7.38.1 S5.1 — conkey/confkey are REAL smallint[] now (they
        // were `{1,2}` text): pg_dump's not-null pass joins on
        // `co.conkey = array[a.attnum]`, and text never equals an
        // integer array. The rendered form is unchanged (`{1,2}`).
        let conkey_vec = |positions: &[usize]| -> Value<'static> {
            Value::SmallIntArray(positions.iter().map(|p| Some(*p as i16 + 1)).collect())
        };
        // Uniqueness constraints.
        for (ui, uc) in t.schema().uniqueness_constraints.iter().enumerate() {
            let kind = if uc.is_primary_key { "p" } else { "u" };
            let conindid = constraint_index
                .get(&(tname.clone(), ui))
                .copied()
                .unwrap_or(0);
            let conname = pg_unique_conname(t, uc, tname);
            let conkey_display = conkey_vec(&uc.columns);
            rows.push(Row::new(alloc::vec![
                Value::BigInt(next_con_oid()),
                Value::text(conname),
                Value::BigInt(2200),
                char1(kind), // contype
                // v7.39 (round 711) — real, now that the flags are stored.
                Value::Bool(uc.deferrable),         // condeferrable
                Value::Bool(uc.initially_deferred), // condeferred
                Value::Bool(true),                  // conenforced
                Value::Bool(true),                  // convalidated
                Value::BigInt(conrelid),
                Value::BigInt(0), // contypid
                Value::BigInt(conindid),
                Value::BigInt(0),   // conparentid
                Value::BigInt(0),   // confrelid (not an FK)
                char1(" "),         // confupdtype
                char1(" "),         // confdeltype
                char1(" "),         // confmatchtype
                Value::Bool(true),  // conislocal
                Value::SmallInt(0), // coninhcount
                Value::Bool(true),
                Value::Bool(false), /* conperiod */
                // connoinherit
                conkey_display.clone(),
                Value::Null, /* confkey: non-FK */
                Value::Null, /* conpfeqop */
                Value::Null, /* conppeqop */
                Value::Null, /* conffeqop */
                Value::Null, /* confdelsetcols */
                Value::Null, /* conexclop */
                Value::Null, /* conbin */
            ]));
        }
        // v7.39.11 — a bare `CREATE UNIQUE INDEX` is an INDEX, and
        // PostgreSQL creates no constraint for it. This loop used to
        // synthesise one.
        //
        // Reported by sentori against 7.39.10: a schema-diff tool
        // comparing the two sides sees a constraint on ours that does
        // not exist on PostgreSQL's — in their own dump that was
        // `users_email_ci_idx`, an expression index, appearing as a
        // constraint. Measured: `CREATE UNIQUE INDEX m6_ab ON m6 (a,b)`
        // gives PostgreSQL no `pg_constraint` row and gave us one with
        // `contype = 'u'`.
        //
        // An index that BACKS a constraint still gets its row from the
        // uniqueness-constraint loop above, which is where the
        // constraint actually lives.
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
                char1("f"), // contype
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(true), /* conenforced */
                Value::Bool(true),
                Value::BigInt(conrelid),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::BigInt(confrelid),
                char1(upd_action), // confupdtype
                char1(del_action), // confdeltype
                char1("s"),        // confmatchtype: 's' SIMPLE (default)
                Value::Bool(true),
                Value::SmallInt(0), // coninhcount
                Value::Bool(true),
                Value::Bool(false), /* conperiod */
                conkey.clone(),
                confkey.clone(),
                Value::Null, /* conpfeqop */
                Value::Null, /* conppeqop */
                Value::Null, /* conffeqop */
                Value::Null, /* confdelsetcols */
                Value::Null, /* conexclop */
                Value::Null, /* conbin */
            ]));
        }
        // v7.37 U5 — CHECK constraints (contype 'c'). Previously
        // omitted, so pg_constraint enumeration missed every CHECK.
        let check_names = pg_check_connames(t, tname, &t.schema().checks);
        for (conname, check_src) in check_names.into_iter().zip(t.schema().checks.iter()) {
            rows.push(Row::new(alloc::vec![
                Value::BigInt(next_con_oid()),
                Value::text(conname),
                Value::BigInt(2200),
                char1("c"), // contype
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(true), /* conenforced */
                // v7.39 (round 652) — convalidated. `f` for a CHECK added
                // NOT VALID: the rows already in the table were never
                // scanned against it. pg_dump reads this column to decide
                // whether to re-emit the suffix.
                Value::Bool(check_src.validated),
                Value::BigInt(conrelid),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::BigInt(0),
                char1(" "), // confupdtype
                char1(" "), // confdeltype
                char1(" "), // confmatchtype
                Value::Bool(true),
                Value::SmallInt(0), // coninhcount
                // connoinherit: FALSE for CHECK (PG18 differential).
                Value::Bool(false),
                Value::Bool(false), /* conperiod */
                // conkey: a table CHECK constrains no single column
                // here — empty smallint[]; confkey NULL (non-FK).
                Value::SmallIntArray(alloc::vec::Vec::new()),
                Value::Null,
                Value::Null,                         /* conpfeqop */
                Value::Null,                         /* conppeqop */
                Value::Null,                         /* conffeqop */
                Value::Null,                         /* confdelsetcols */
                Value::Null,                         /* conexclop */
                Value::text(check_src.expr.clone()), /* conbin */
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
                char1("x"), // contype
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(true), /* conenforced */
                Value::Bool(true),
                Value::BigInt(conrelid),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::BigInt(0),
                char1(" "), // confupdtype
                char1(" "), // confdeltype
                char1(" "), // confmatchtype
                Value::Bool(true),
                Value::SmallInt(0), // coninhcount
                Value::Bool(true),
                Value::Bool(false), /* conperiod */
                conkey_display.clone(),
                Value::Null, /* confkey: non-FK */
                Value::Null, /* conpfeqop */
                Value::Null, /* conppeqop */
                Value::Null, /* conffeqop */
                Value::Null, /* confdelsetcols */
                Value::Null, /* conexclop */
                Value::Null, /* conbin */
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
            let conkey_display = conkey_vec(&[i]);
            rows.push(Row::new(alloc::vec![
                Value::BigInt(next_con_oid()),
                Value::text(conname),
                Value::BigInt(2200),
                char1("n"), // contype
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(true), /* conenforced */
                Value::Bool(true),
                Value::BigInt(conrelid),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::BigInt(0),
                char1(" "), // confupdtype
                char1(" "), // confdeltype
                char1(" "), // confmatchtype
                Value::Bool(true),
                Value::SmallInt(0), // coninhcount
                // connoinherit: FALSE for NOT NULL (PG18 differential:
                // c=f, n=f, p=t, u=t). Claiming true printed
                // `NOT NULL NO INHERIT` on every dumped column, which
                // SPG's own restore refused (7.38.1 S5.2).
                Value::Bool(false),
                Value::Bool(false), /* conperiod */
                conkey_display.clone(),
                Value::Null, /* confkey: non-FK */
                Value::Null, /* conpfeqop */
                Value::Null, /* conppeqop */
                Value::Null, /* conffeqop */
                Value::Null, /* confdelsetcols */
                Value::Null, /* conexclop */
                Value::Null, /* conbin */
            ]));
        }
    }
    // 8.0.3 — a domain's constraints: its CHECKs ('c') and its NOT NULL
    // ('n', named `<domain>_not_null`), with `contypid` naming the domain
    // and no relation (measured on 18.6). There were none, and `pg_dump`
    // reads a domain's constraints from exactly these rows: every domain
    // dumped without its CHECK, and restored accepting what it had refused.
    //
    // Numbered in a band of their own. Following on from the table
    // constraints made their oids depend on how many tables the catalog
    // holds, and `pg_get_constraintdef` runs against a catalog that also
    // holds the materialised system views — so it resolved the domain's
    // CHECK to some table's NOT NULL (`NOT NULL oid`).
    let (_, _, domains) = user_type_oids(cat);
    let mut domain_con_oid = OID_DOMAIN_CONSTRAINT_BASE;
    for (name, typid) in domains {
        let Some(d) = cat.domain_types().get(&name) else {
            continue;
        };
        let mut cons: Vec<(String, &'static str, Option<String>)> = d
            .checks
            .iter()
            .map(|c| (c.name.clone(), "c", Some(c.expr.clone())))
            .collect();
        if !d.nullable {
            cons.push((alloc::format!("{name}_not_null"), "n", None));
        }
        for (conname, kind, bin) in cons {
            rows.push(Row::new(alloc::vec![
                Value::BigInt({
                    domain_con_oid += 1;
                    domain_con_oid
                }),
                Value::text(conname),
                Value::BigInt(2200),
                char1(kind), // contype
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(true), /* conenforced */
                Value::Bool(true), /* convalidated */
                Value::BigInt(0),  /* conrelid — no relation */
                Value::BigInt(typid),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::BigInt(0),
                char1(" "), // confupdtype
                char1(" "), // confdeltype
                char1(" "), // confmatchtype
                Value::Bool(true),
                Value::SmallInt(0), // coninhcount
                Value::Bool(false),
                Value::Bool(false), /* conperiod */
                Value::SmallIntArray(alloc::vec::Vec::new()),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                bin.map_or(Value::Null, Value::text),
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
/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows.
pub(crate) fn pg_database_schema() -> Vec<ColumnSchema> {
    alloc::vec![
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("datname", DataType::Text, false),
        ColumnSchema::new("datdba", DataType::BigInt, false),
        ColumnSchema::new("encoding", DataType::Int, false),
        ColumnSchema::new("datlocprovider", DataType::Char1, false),
        ColumnSchema::new("datistemplate", DataType::Bool, false),
        ColumnSchema::new("datallowconn", DataType::Bool, false),
        ColumnSchema::new("dathasloginevt", DataType::Bool, false),
        ColumnSchema::new("datconnlimit", DataType::Int, false),
        ColumnSchema::new("datfrozenxid", DataType::Xid, false),
        ColumnSchema::new("datminmxid", DataType::BigInt, false),
        ColumnSchema::new("dattablespace", DataType::BigInt, false),
        ColumnSchema::new("datcollate", DataType::Text, false),
        ColumnSchema::new("datctype", DataType::Text, false),
        ColumnSchema::new("datlocale", DataType::Text, true),
        ColumnSchema::new("daticurules", DataType::Text, true),
        ColumnSchema::new("datcollversion", DataType::Text, true),
        ColumnSchema::new("datacl", DataType::Text, true),
    ]
}

pub(crate) fn synth_pg_database(engine: &Engine) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    // v7.39 (round 474) — PG18's pg_database has eighteen columns; this had
    // five, so `SELECT datfrozenxid FROM pg_database` — what a monitoring
    // query asks to watch wraparound — failed outright with "column does
    // not exist".
    //
    // It also named the database `postgres` while `current_database()`
    // answers `spg`, so a client joining the two found no row at all.
    let schema = pg_database_schema();
    // v7.38.19 — every name this server answers to, not only the one the
    // asking session connected with.
    //
    // SPG serves one database and accepts any name for it, so a
    // `CREATE DATABASE dd` succeeds and `dd` connects — and this table
    // listed exactly one row, whichever name the current session used.
    // A database that had just been created and could be connected to
    // was absent from the catalogue. `psql \l`, a migration tool asking
    // "does this database exist", and a backup script that enumerates
    // all read this table. Reported by sentori against 7.38.18.
    //
    // The names are aliases onto one database, so every row carries the
    // same collation and the same frozen xid. That is the honest
    // rendering of what SPG is; showing one row was not.
    let connected = engine
        .session_params
        .get("spg.database")
        .cloned()
        .unwrap_or_else(|| alloc::string::String::from("spg"));
    let mut names: alloc::vec::Vec<alloc::string::String> = alloc::vec![connected.clone()];
    for n in engine.catalog.created_databases() {
        if !names.iter().any(|k| k == n) {
            names.push(n.clone());
        }
    }
    let rows = names
        .into_iter()
        .enumerate()
        .map(|(i, dbname)| {
            Row::new(alloc::vec![
                Value::BigInt(16384 + i as i64),
                // The name `current_database()` answers, read from the same place
                // it reads, so the two cannot drift apart again.
                Value::text(dbname),
                Value::BigInt(10),
                Value::Int(6), // UTF8
                // 'c' = libc provider, which is what SPG's C collation is.
                char1("c"),         // datlocprovider — libc
                Value::Bool(false), // datistemplate
                Value::Bool(true),  // datallowconn
                Value::Bool(false), // dathasloginevt
                Value::Int(-1),     // datconnlimit — unlimited
                // The real MVCC floor, not a placeholder: this is the number a
                // wraparound monitor is watching, and SPG has one to give.
                // v7.39 (round 640) — an `xid`, same as pg_class.relfrozenxid.
                Value::Xid(u32::try_from(engine.vacuum_oldest_active()).unwrap_or(u32::MAX)),
                Value::BigInt(1),    // datminmxid — SPG has no multixact
                Value::BigInt(1663), // dattablespace — pg_default
                // v7.38.18 (S1) — what this database was actually created with,
                // not the constant `C` that stood here while there was no way to
                // create it with anything else. A client reads these to decide
                // how the server sorts.
                Value::text::<String>(engine.database_collation().into()),
                Value::text::<String>(engine.database_collation().into()),
                Value::Null, // datlocale
                Value::Null, // daticurules
                Value::Null, // datcollversion
                Value::Null, // datacl
            ])
        })
        .collect();
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-55 — synthesise `pg_catalog.pg_roles`. PG's
/// pg_roles is a view over pg_authid showing all roles. SPG ships
/// one row per declared user from the engine's UserStore so admin
/// tool startup screens can populate.
/// v7.39 (round 541 sweep, round 542) — PG18's thirteen columns, in
/// PG's order.
///
/// `oid` is LAST in PG's pg_roles, not first, and eight columns were
/// missing. SPG records three role attributes (superuser, inherit,
/// canlogin); it ACCEPTS the rest of PG's options and does not store
/// them, so it reports what it will actually enforce. A superuser
/// bypasses every one of those checks, which is why PG reports them
/// true for one.
///
/// rolpassword reads `********` for everybody, as PG's does — the view
/// exists so that the hash does NOT leave the catalog.
/// 9.0.0 — the shape alone, so the catalog can list this
/// relation's columns without building its rows. See `CATALOG_RELATIONS`.
pub(crate) fn pg_roles_schema() -> Vec<ColumnSchema> {
    alloc::vec![
        ColumnSchema::new("rolname", DataType::Name, false),
        ColumnSchema::new("rolsuper", DataType::Bool, false),
        ColumnSchema::new("rolinherit", DataType::Bool, false),
        ColumnSchema::new("rolcreaterole", DataType::Bool, false),
        ColumnSchema::new("rolcreatedb", DataType::Bool, false),
        ColumnSchema::new("rolcanlogin", DataType::Bool, false),
        ColumnSchema::new("rolreplication", DataType::Bool, false),
        ColumnSchema::new("rolconnlimit", DataType::Int, false),
        ColumnSchema::new("rolpassword", DataType::Text, true),
        ColumnSchema::new("rolvaliduntil", DataType::Timestamptz, true),
        ColumnSchema::new("rolbypassrls", DataType::Bool, false),
        ColumnSchema::new("rolconfig", DataType::TextArray, true),
        ColumnSchema::new("oid", DataType::Oid, false),
    ]
}

pub(crate) fn synth_pg_roles(engine: &Engine) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_roles_schema();
    // 8.0.3 — every role, numbered once (see `role_directory`). The
    // bootstrap superuser is oid 10 and PostgreSQL's predefined roles carry
    // their own oids; `postgres` is listed only when it IS the bootstrap
    // role, as in PostgreSQL started with another POSTGRES_USER.
    //
    // v7.39 (read01 round 58) — the three attributes are REAL. They used to
    // be hard-coded (`false, true, true`) because SPG had no role attributes;
    // a `CREATE ROLE devs NOLOGIN` would still have reported rolcanlogin=true.
    //
    // v7.39 (round 696) — and the SESSION's own identity is listed: a
    // pg-wire client authenticates as some user, `current_user` reports
    // that name, and `pg_roles` has to list it for `'bench'::regrole` and
    // `SET ROLE bench` to agree.
    let rows = crate::role_directory::RoleDirectory::of(engine)
        .entries()
        .iter()
        .map(|r| pg_roles_row(r.oid, &r.name, r.superuser, r.inherit, r.can_login))
        .collect();
    (schema, rows)
}

/// One pg_roles row, so pg_user cannot drift from it.
fn pg_roles_row(
    oid: i64,
    name: &str,
    superuser: bool,
    inherit: bool,
    can_login: bool,
) -> Row<'static> {
    Row::new(alloc::vec![
        Value::text(alloc::string::String::from(name)),
        Value::Bool(superuser),
        Value::Bool(inherit),
        // SPG does not record these four; a superuser bypasses the
        // checks they gate, which is why PG reports them true for one.
        Value::Bool(superuser), // rolcreaterole
        Value::Bool(superuser), // rolcreatedb
        Value::Bool(can_login),
        Value::Bool(superuser), // rolreplication
        Value::Int(-1),         // rolconnlimit — unlimited
        Value::text("********"),
        Value::Null, // rolvaliduntil
        Value::Bool(superuser),
        Value::Null, // rolconfig
        Value::BigInt(oid),
    ])
}

/// v7.39 (round 542) — `pg_catalog.pg_user`.
///
/// SPG published pg_roles' columns under this name, so PG's own
/// spelling of the most ordinary question there is —
/// `SELECT usename FROM pg_user` — failed with "column does not
/// exist". PG's pg_user is a DIFFERENT view over the same roles: only
/// the ones that can log in, with `use*` names.
///
/// Derived from synth_pg_roles' rows so the two cannot disagree.
/// 9.0.0 — the shape alone, so the catalog can list this
/// relation's columns without building its rows. See `CATALOG_RELATIONS`.
pub(crate) fn pg_user_schema() -> Vec<ColumnSchema> {
    alloc::vec![
        ColumnSchema::new("usename", DataType::Name, false),
        ColumnSchema::new("usesysid", DataType::Oid, false),
        ColumnSchema::new("usecreatedb", DataType::Bool, false),
        ColumnSchema::new("usesuper", DataType::Bool, false),
        ColumnSchema::new("userepl", DataType::Bool, false),
        ColumnSchema::new("usebypassrls", DataType::Bool, false),
        ColumnSchema::new("passwd", DataType::Text, true),
        ColumnSchema::new("valuntil", DataType::Timestamptz, true),
        ColumnSchema::new("useconfig", DataType::TextArray, true),
    ]
}

pub(crate) fn synth_pg_user(engine: &Engine) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_user_schema();
    // pg_roles positions: 0 rolname, 1 rolsuper, 4 rolcreatedb,
    // 5 rolcanlogin, 6 rolreplication, 10 rolbypassrls, 12 oid.
    let (_, roles) = synth_pg_roles(engine);
    let rows = roles
        .into_iter()
        .filter(|r| matches!(r.values[5], Value::Bool(true)))
        .map(|r| {
            Row::new(alloc::vec![
                r.values[0].clone(),
                r.values[12].clone(),
                r.values[4].clone(),
                r.values[1].clone(),
                r.values[6].clone(),
                r.values[10].clone(),
                Value::text("********"),
                Value::Null,
                Value::Null,
            ])
        })
        .collect();
    (schema, rows)
}

/// v7.39 (read01 round 58) — synthesise `pg_catalog.pg_auth_members`: one row
/// per role membership (`GRANT devs TO alice`). The oids agree with the ones
/// `synth_pg_roles` hands out, so the canonical
/// `pg_auth_members JOIN pg_roles ON roleid = oid` join resolves.
/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows. The rows and the
/// columns come from one place, so `pg_attribute` cannot describe
/// a relation differently from how it answers.
pub(crate) fn pg_auth_members_schema() -> Vec<ColumnSchema> {
    alloc::vec![
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("roleid", DataType::BigInt, false),
        ColumnSchema::new("member", DataType::BigInt, false),
        ColumnSchema::new("grantor", DataType::BigInt, false),
        ColumnSchema::new("admin_option", DataType::Bool, false),
        // v7.39 (round 543) — PG16+ split these out of admin_option.
        // Measured true/true for a plain GRANT <role> TO <member>.
        ColumnSchema::new("inherit_option", DataType::Bool, false),
        ColumnSchema::new("set_option", DataType::Bool, false),
    ]
}

pub(crate) fn synth_pg_auth_members(engine: &Engine) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_auth_members_schema();
    // The oid assignment synth_pg_roles publishes.
    let roles = crate::role_directory::RoleDirectory::of(engine);
    let oid_of = |name: &str| -> i64 {
        roles
            .oid_of(name)
            .unwrap_or(crate::role_directory::BOOTSTRAP_OID)
    };
    let mut rows: Vec<Row<'static>> = Vec::new();
    for (i, (member, role)) in engine.users.all_memberships().enumerate() {
        rows.push(Row::new(alloc::vec![
            Value::BigInt(200_000 + (i as i64)),
            Value::BigInt(oid_of(role)),
            Value::BigInt(oid_of(member)),
            Value::BigInt(10), // grantor — the bootstrap superuser
            Value::Bool(false),
            Value::Bool(true), // inherit_option
            Value::Bool(true), // set_option
        ]));
    }
    (schema, rows)
}

/// v7.17.0 Phase 3.P0-56 — synthesise `pg_catalog.pg_views`. PG's
/// pg_views is a view listing every catalog view; SPG ships one
/// row per declared view + its definition text.
/// v7.39 (round 541) — the pg_catalog relations that exist in PG and
/// are genuinely EMPTY in SPG.
///
/// Sweeping PG18's 144 pg_catalog relations against SPG found 69 that
/// SPG does not have at all. That is not the same as having none of a
/// thing: a catalog that exists and returns no rows lets a tool
/// conclude "no foreign tables here"; a catalog that does not exist
/// stops it. `pg_dump` demonstrated both, one query at a time.
///
/// These twenty-eight are empty because the feature behind them is
/// absent from SPG by design — no foreign-data wrappers, no security
/// labels, no prepared transactions, no shared-memory segments, no
/// configuration FILES to report rules from. Publishing them empty is
/// the honest answer, and it is PG's answer on a database that has
/// none of those either.
///
/// The rest of the 69 (pg_language, pg_operator, pg_opclass,
/// pg_opfamily, pg_range, pg_sequences, pg_partitioned_table,
/// pg_group, pg_shadow, the pg_ts_* family, and the pg_stat_* /
/// pg_statio_* all/sys variants) would NOT be empty — SPG has
/// sequences, partitions, ranges, roles, full-text search and
/// statistics. Stubbing those empty would be a lie, so they are
/// recorded as work rather than filled in here.
///
/// Column names and types are PG18 readings.
const EMPTY_PG_CATALOGS: &[(&str, &[(&str, DataType)])] = &[
    // v7.39 (round 546) — two more PG catalogs SPG is genuinely empty
    // of: it has no ALTER DEFAULT PRIVILEGES and one encoding, so there
    // are no conversions between any.
    (
        "pg_default_acl",
        &[
            ("oid", DataType::BigInt),
            ("defaclrole", DataType::BigInt),
            ("defaclnamespace", DataType::BigInt),
            ("defaclobjtype", DataType::Text),
            ("defaclacl", DataType::TextArray),
        ],
    ),
    (
        "pg_conversion",
        &[
            ("oid", DataType::BigInt),
            ("conname", DataType::Text),
            ("connamespace", DataType::BigInt),
            ("conowner", DataType::BigInt),
            ("conforencoding", DataType::Int),
            ("contoencoding", DataType::Int),
            ("conproc", DataType::BigInt),
            ("condefault", DataType::Bool),
        ],
    ),
    (
        "pg_event_trigger",
        &[
            ("oid", DataType::BigInt),
            ("evtname", DataType::Text),
            ("evtevent", DataType::Text),
            ("evtowner", DataType::BigInt),
            ("evtfoid", DataType::BigInt),
            ("evtenabled", DataType::Text),
            ("evttags", DataType::TextArray),
        ],
    ),
    (
        "pg_file_settings",
        &[
            ("sourcefile", DataType::Text),
            ("sourceline", DataType::Int),
            ("seqno", DataType::Int),
            ("name", DataType::Text),
            ("setting", DataType::Text),
            ("applied", DataType::Bool),
            ("error", DataType::Text),
        ],
    ),
    (
        "pg_foreign_data_wrapper",
        &[
            ("oid", DataType::BigInt),
            ("fdwname", DataType::Text),
            ("fdwowner", DataType::BigInt),
            ("fdwhandler", DataType::BigInt),
            ("fdwvalidator", DataType::BigInt),
            ("fdwacl", DataType::TextArray),
            ("fdwoptions", DataType::TextArray),
        ],
    ),
    (
        "pg_foreign_server",
        &[
            ("oid", DataType::BigInt),
            ("srvname", DataType::Text),
            ("srvowner", DataType::BigInt),
            ("srvfdw", DataType::BigInt),
            ("srvtype", DataType::Text),
            ("srvversion", DataType::Text),
            ("srvacl", DataType::TextArray),
            ("srvoptions", DataType::TextArray),
        ],
    ),
    (
        "pg_hba_file_rules",
        &[
            ("rule_number", DataType::Int),
            ("file_name", DataType::Text),
            ("line_number", DataType::Int),
            ("type", DataType::Text),
            ("database", DataType::TextArray),
            ("user_name", DataType::TextArray),
            ("address", DataType::Text),
            ("netmask", DataType::Text),
            ("auth_method", DataType::Text),
            ("options", DataType::TextArray),
            ("error", DataType::Text),
        ],
    ),
    (
        "pg_ident_file_mappings",
        &[
            ("map_number", DataType::Int),
            ("file_name", DataType::Text),
            ("line_number", DataType::Int),
            ("map_name", DataType::Text),
            ("sys_name", DataType::Text),
            ("pg_username", DataType::Text),
            ("error", DataType::Text),
        ],
    ),
    (
        "pg_init_privs",
        &[
            ("objoid", DataType::BigInt),
            ("classoid", DataType::BigInt),
            ("objsubid", DataType::Int),
            ("privtype", DataType::Text),
            ("initprivs", DataType::TextArray),
        ],
    ),
    (
        "pg_parameter_acl",
        &[
            ("oid", DataType::BigInt),
            ("parname", DataType::Text),
            ("paracl", DataType::TextArray),
        ],
    ),
    (
        "pg_prepared_xacts",
        &[
            ("transaction", DataType::BigInt),
            ("gid", DataType::Text),
            ("prepared", DataType::Timestamptz),
            ("owner", DataType::Text),
            ("database", DataType::Text),
        ],
    ),
    (
        "pg_publication_namespace",
        &[
            ("oid", DataType::BigInt),
            ("pnpubid", DataType::BigInt),
            ("pnnspid", DataType::BigInt),
        ],
    ),
    (
        "pg_publication_rel",
        &[
            ("oid", DataType::BigInt),
            ("prpubid", DataType::BigInt),
            ("prrelid", DataType::BigInt),
            ("prqual", DataType::Text),
            ("prattrs", DataType::Text),
        ],
    ),
    (
        "pg_publication_tables",
        &[
            ("pubname", DataType::Text),
            ("schemaname", DataType::Text),
            ("tablename", DataType::Text),
            ("attnames", DataType::TextArray),
            ("rowfilter", DataType::Text),
        ],
    ),
    (
        "pg_replication_origin",
        &[("roident", DataType::BigInt), ("roname", DataType::Text)],
    ),
    (
        "pg_replication_origin_status",
        &[
            ("local_id", DataType::BigInt),
            ("external_id", DataType::Text),
            ("remote_lsn", DataType::Text),
            ("local_lsn", DataType::Text),
        ],
    ),
    (
        "pg_seclabel",
        &[
            ("objoid", DataType::BigInt),
            ("classoid", DataType::BigInt),
            ("objsubid", DataType::Int),
            ("provider", DataType::Text),
            ("label", DataType::Text),
        ],
    ),
    (
        "pg_seclabels",
        &[
            ("objoid", DataType::BigInt),
            ("classoid", DataType::BigInt),
            ("objsubid", DataType::Int),
            ("objtype", DataType::Text),
            ("objnamespace", DataType::BigInt),
            ("objname", DataType::Text),
            ("provider", DataType::Text),
            ("label", DataType::Text),
        ],
    ),
    (
        "pg_shdepend",
        &[
            ("dbid", DataType::BigInt),
            ("classid", DataType::BigInt),
            ("objid", DataType::BigInt),
            ("objsubid", DataType::Int),
            ("refclassid", DataType::BigInt),
            ("refobjid", DataType::BigInt),
            ("deptype", DataType::Char1),
        ],
    ),
    (
        "pg_shdescription",
        &[
            ("objoid", DataType::BigInt),
            ("classoid", DataType::BigInt),
            ("description", DataType::Text),
        ],
    ),
    (
        "pg_shmem_allocations",
        &[
            ("name", DataType::Text),
            ("off", DataType::BigInt),
            ("size", DataType::BigInt),
            ("allocated_size", DataType::BigInt),
        ],
    ),
    (
        "pg_shmem_allocations_numa",
        &[
            ("name", DataType::Text),
            ("numa_node", DataType::Int),
            ("size", DataType::BigInt),
        ],
    ),
    (
        "pg_shseclabel",
        &[
            ("objoid", DataType::BigInt),
            ("classoid", DataType::BigInt),
            ("provider", DataType::Text),
            ("label", DataType::Text),
        ],
    ),
    (
        "pg_statistic_ext_data",
        &[
            ("stxoid", DataType::BigInt),
            ("stxdinherit", DataType::Bool),
            ("stxdndistinct", DataType::Text),
            ("stxddependencies", DataType::Text),
            ("stxdmcv", DataType::Text),
            ("stxdexpr", DataType::TextArray),
        ],
    ),
    (
        "pg_stats_ext",
        &[
            ("schemaname", DataType::Text),
            ("tablename", DataType::Text),
            ("statistics_schemaname", DataType::Text),
            ("statistics_name", DataType::Text),
            ("statistics_owner", DataType::Text),
            ("attnames", DataType::TextArray),
            ("exprs", DataType::TextArray),
            ("kinds", DataType::TextArray),
            ("inherited", DataType::Bool),
            ("n_distinct", DataType::Text),
            ("dependencies", DataType::Text),
            ("most_common_vals", DataType::TextArray),
            ("most_common_val_nulls", DataType::BoolArray),
            ("most_common_freqs", DataType::FloatArray),
            ("most_common_base_freqs", DataType::FloatArray),
        ],
    ),
    (
        "pg_stats_ext_exprs",
        &[
            ("schemaname", DataType::Text),
            ("tablename", DataType::Text),
            ("statistics_schemaname", DataType::Text),
            ("statistics_name", DataType::Text),
            ("statistics_owner", DataType::Text),
            ("expr", DataType::Text),
            ("inherited", DataType::Bool),
            ("null_frac", DataType::Float),
            ("avg_width", DataType::Int),
            ("n_distinct", DataType::Float),
            ("most_common_vals", DataType::TextArray),
            ("most_common_freqs", DataType::FloatArray),
            ("histogram_bounds", DataType::TextArray),
            ("correlation", DataType::Float),
            ("most_common_elems", DataType::TextArray),
            ("most_common_elem_freqs", DataType::FloatArray),
            ("elem_count_histogram", DataType::FloatArray),
        ],
    ),
    (
        "pg_subscription_rel",
        &[
            ("srsubid", DataType::BigInt),
            ("srrelid", DataType::BigInt),
            ("srsubstate", DataType::Text),
            ("srsublsn", DataType::Text),
        ],
    ),
    (
        "pg_transform",
        &[
            ("oid", DataType::BigInt),
            ("trftype", DataType::BigInt),
            ("trflang", DataType::BigInt),
            ("trffromsql", DataType::Text),
            ("trftosql", DataType::Text),
        ],
    ),
    (
        "pg_user_mapping",
        &[
            ("oid", DataType::BigInt),
            ("umuser", DataType::BigInt),
            ("umserver", DataType::BigInt),
            ("umoptions", DataType::TextArray),
        ],
    ),
    (
        "pg_user_mappings",
        &[
            ("umid", DataType::BigInt),
            ("srvid", DataType::BigInt),
            ("srvname", DataType::Text),
            ("umuser", DataType::BigInt),
            ("usename", DataType::Text),
            ("umoptions", DataType::TextArray),
        ],
    ),
];

/// Look up an empty catalog by its `__spg_pg_`-rewritten name.
pub(crate) fn synth_empty_pg_catalog(view: &str) -> Option<(Vec<ColumnSchema>, Vec<Row<'static>>)> {
    let bare = view.strip_prefix("__spg_pg_")?;
    let (_, cols) = EMPTY_PG_CATALOGS
        .iter()
        .find(|(name, _)| name.strip_prefix("pg_") == Some(bare))?;
    let schema = cols
        .iter()
        .map(|(n, t)| ColumnSchema::new(*n, *t, true))
        .collect();
    Some((schema, Vec::new()))
}

/// v7.39 (round 546) — the three role views PG derives from pg_authid,
/// all built from synth_pg_roles' rows so none of them can drift.
///
/// `pg_authid` is where PG keeps the password hash; `pg_shadow` is the
/// same for login roles. **SPG masks both** — it reports `********`,
/// as pg_roles does. PG guards the real hash with catalog-level
/// privileges that SPG does not have, so publishing a SCRAM verifier
/// here would put it within reach of any session. That is a deliberate
/// divergence, recorded rather than silently taken.
/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows. The rows and the
/// columns come from one place, so `pg_attribute` cannot describe
/// a relation differently from how it answers.
pub(crate) fn pg_authid_schema() -> Vec<ColumnSchema> {
    alloc::vec![
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("rolname", DataType::Name, false),
        ColumnSchema::new("rolsuper", DataType::Bool, false),
        ColumnSchema::new("rolinherit", DataType::Bool, false),
        ColumnSchema::new("rolcreaterole", DataType::Bool, false),
        ColumnSchema::new("rolcreatedb", DataType::Bool, false),
        ColumnSchema::new("rolcanlogin", DataType::Bool, false),
        ColumnSchema::new("rolreplication", DataType::Bool, false),
        ColumnSchema::new("rolbypassrls", DataType::Bool, false),
        ColumnSchema::new("rolconnlimit", DataType::Int, false),
        ColumnSchema::new("rolpassword", DataType::Text, true),
        ColumnSchema::new("rolvaliduntil", DataType::Timestamptz, true),
    ]
}

pub(crate) fn synth_pg_authid(engine: &Engine) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_authid_schema();
    // pg_roles positions: 0 rolname, 1 rolsuper, 2 rolinherit,
    // 3 rolcreaterole, 4 rolcreatedb, 5 rolcanlogin, 6 rolreplication,
    // 7 rolconnlimit, 8 rolpassword, 9 rolvaliduntil, 10 rolbypassrls,
    // 12 oid.
    let (_, roles) = synth_pg_roles(engine);
    let rows = roles
        .into_iter()
        .map(|r| {
            Row::new(alloc::vec![
                r.values[12].clone(),
                r.values[0].clone(),
                r.values[1].clone(),
                r.values[2].clone(),
                r.values[3].clone(),
                r.values[4].clone(),
                r.values[5].clone(),
                r.values[6].clone(),
                r.values[10].clone(),
                r.values[7].clone(),
                r.values[8].clone(),
                r.values[9].clone(),
            ])
        })
        .collect();
    (schema, rows)
}

/// `pg_group` — a role and the oids of its members.
/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows. The rows and the
/// columns come from one place, so `pg_attribute` cannot describe
/// a relation differently from how it answers.
pub(crate) fn pg_group_schema() -> Vec<ColumnSchema> {
    alloc::vec![
        ColumnSchema::new("groname", DataType::Text, false),
        ColumnSchema::new("grosysid", DataType::BigInt, false),
        ColumnSchema::new("grolist", DataType::TextArray, true),
    ]
}

pub(crate) fn synth_pg_group(engine: &Engine) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_group_schema();
    let (_, members) = synth_pg_auth_members(engine);
    let (_, roles) = synth_pg_roles(engine);
    let rows = roles
        .into_iter()
        .map(|r| {
            let oid = match &r.values[12] {
                Value::BigInt(o) => *o,
                _ => 0,
            };
            // pg_auth_members positions: 1 roleid, 2 member.
            let list: Vec<Option<alloc::string::String>> = members
                .iter()
                .filter(|m| matches!(&m.values[1], Value::BigInt(o) if *o == oid))
                .map(|m| {
                    Some(alloc::format!(
                        "{}",
                        crate::eval::value_to_text(&m.values[2])
                    ))
                })
                .collect();
            Row::new(alloc::vec![
                r.values[0].clone(),
                Value::BigInt(oid),
                if list.is_empty() {
                    Value::Null
                } else {
                    Value::TextArray(list)
                },
            ])
        })
        .collect();
    (schema, rows)
}

/// `pg_shadow` — pg_user's columns, for the login roles, with the
/// password column PG reserves for superusers. Masked here; see
/// synth_pg_authid.
pub(crate) fn synth_pg_shadow(engine: &Engine) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    synth_pg_user(engine)
}

/// v7.39 (round 547) — `pg_catalog.pg_db_role_setting`, for real.
///
/// Round 546 put this in the empty family on the grounds that SPG
/// recorded no per-role settings. It did not record them because
/// `ALTER ROLE … SET` fell into the parser's pg_dump no-op tail — the
/// statement reported success and changed nothing. It records them now.
///
/// PG's keying, measured: setdatabase and setrole are oid 0 for "all",
/// so `ALTER ROLE ALL SET` is (0, 0), `ALTER DATABASE d SET` is (d, 0),
/// `ALTER ROLE r SET` is (0, r) and `ALTER ROLE r IN DATABASE d SET` is
/// (d, r).
/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows. The rows and the
/// columns come from one place, so `pg_attribute` cannot describe
/// a relation differently from how it answers.
pub(crate) fn pg_db_role_setting_schema() -> Vec<ColumnSchema> {
    alloc::vec![
        ColumnSchema::new("setdatabase", DataType::BigInt, false),
        ColumnSchema::new("setrole", DataType::BigInt, false),
        ColumnSchema::new("setconfig", DataType::TextArray, true),
    ]
}

pub(crate) fn synth_pg_db_role_setting(engine: &Engine) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_db_role_setting_schema();
    let (_, roles) = synth_pg_roles(engine);
    let role_oid = |name: &str| -> i64 {
        roles
            .iter()
            .find(|r| matches!(&r.values[0], Value::Text(n) if n.eq_ignore_ascii_case(name)))
            .and_then(|r| match &r.values[12] {
                Value::BigInt(o) => Some(*o),
                _ => None,
            })
            .unwrap_or(0)
    };
    let rows = engine
        .active_catalog()
        .db_role_settings()
        .iter()
        .map(|((db, role), params)| {
            let config: Vec<Option<alloc::string::String>> = params
                .iter()
                .map(|(k, v)| Some(alloc::format!("{k}={v}")))
                .collect();
            Row::new(alloc::vec![
                // SPG has one database; its oid is the one pg_database
                // publishes.
                Value::BigInt(if db.is_empty() { 0 } else { 16384 }),
                Value::BigInt(if role.is_empty() { 0 } else { role_oid(role) }),
                Value::TextArray(config),
            ])
        })
        .collect();
    (schema, rows)
}

/// v7.39 (round 546) — `pg_catalog.pg_language`: the languages a
/// CREATE FUNCTION here can actually name.
///
/// PG ships four (internal, c, sql, plpgsql); SPG runs three of them —
/// its builtins report prolang 12 (internal), and it executes `sql` and
/// `plpgsql` bodies. `c` is not listed: SPG cannot load a shared
/// object, and a row here would claim it could.
///
/// PG's oids, measured: internal 12, c 13, sql 14, plpgsql 13647.
/// lanispl is true only for a loadable language; lanpltrusted is true
/// for sql and plpgsql. The three handler oids are 0 — those functions
/// are not catalogued here, the same reasoning round 543 applied to
/// pg_type's typinput.
/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows. The rows and the
/// columns come from one place, so `pg_attribute` cannot describe
/// a relation differently from how it answers.
pub(crate) fn pg_language_schema() -> Vec<ColumnSchema> {
    alloc::vec![
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("lanname", DataType::Text, false),
        ColumnSchema::new("lanowner", DataType::BigInt, false),
        ColumnSchema::new("lanispl", DataType::Bool, false),
        ColumnSchema::new("lanpltrusted", DataType::Bool, false),
        ColumnSchema::new("lanplcallfoid", DataType::BigInt, false),
        ColumnSchema::new("laninline", DataType::BigInt, false),
        ColumnSchema::new("lanvalidator", DataType::BigInt, false),
        ColumnSchema::new("lanacl", DataType::Text, true),
    ]
}

pub(crate) fn synth_pg_language() -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_language_schema();
    let row = |oid: i64, name: &'static str, ispl: bool, trusted: bool| {
        Row::new(alloc::vec![
            Value::BigInt(oid),
            Value::text(name),
            Value::BigInt(10),
            Value::Bool(ispl),
            Value::Bool(trusted),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::BigInt(0),
            Value::Null,
        ])
    };
    let rows = alloc::vec![
        row(12, "internal", false, false),
        row(14, "sql", false, true),
        row(13647, "plpgsql", true, true),
    ];
    (schema, rows)
}

/// v7.39 (round 546) — `pg_catalog.pg_sequences`, the listing view.
///
/// SPG had the raw `pg_sequence` catalog and not the view every tool
/// and every human actually reads, so `SELECT * FROM pg_sequences` —
/// the ordinary way to see what a schema's sequences are set to — said
/// the relation did not exist. Derived from the same SequenceDef rows
/// pg_sequence publishes, so the two cannot disagree.
///
/// Measured on PG18 for `CREATE SEQUENCE s`: last_value is NULL until
/// the sequence has been called.
pub(crate) fn synth_pg_sequences(
    cat: &Catalog,
    roles: &crate::role_directory::RoleDirectory,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("schemaname", DataType::Name, false),
        ColumnSchema::new("sequencename", DataType::Name, false),
        ColumnSchema::new("sequenceowner", DataType::Name, false),
        ColumnSchema::new("data_type", DataType::RegType, false),
        ColumnSchema::new("start_value", DataType::BigInt, false),
        ColumnSchema::new("min_value", DataType::BigInt, false),
        ColumnSchema::new("max_value", DataType::BigInt, false),
        ColumnSchema::new("increment_by", DataType::BigInt, false),
        ColumnSchema::new("cycle", DataType::Bool, false),
        ColumnSchema::new("cache_size", DataType::BigInt, false),
        ColumnSchema::new("last_value", DataType::BigInt, true),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for (stored, def) in crate::sequence::catalog_sequences(cat) {
        let def = def.as_ref();
        let Some(name) = cat.listed_name(&stored) else {
            continue;
        };
        rows.push(Row::new(alloc::vec![
            // 9.0.0 (C9) — the sequence's own schema, not `public` for
            // every one of them.
            Value::text(alloc::string::String::from(cat.listed_schema(&stored))),
            Value::text(alloc::string::String::from(name)),
            Value::text(String::from(roles.owner_name(sequence_owner(cat, def)))),
            Value::text("bigint"),
            Value::BigInt(def.start),
            Value::BigInt(def.min_value),
            Value::BigInt(def.max_value),
            Value::BigInt(def.increment),
            Value::Bool(def.cycle),
            Value::BigInt(def.cache),
            // NULL until the sequence has been called, as PG's is.
            if def.is_called {
                Value::BigInt(def.last_value)
            } else {
                Value::Null
            },
        ]));
    }
    (schema, rows)
}

/// v7.39 (round 546) — `pg_catalog.pg_range`.
///
/// SPG has the same six range types PG18 ships, so this is exact.
/// The three function columns and rngsubopc read 0: SPG has no
/// pg_operator / pg_opclass to name, and no multirange types, which is
/// what rngmultitypid would point at.
/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows. The rows and the
/// columns come from one place, so `pg_attribute` cannot describe
/// a relation differently from how it answers.
pub(crate) fn pg_range_schema() -> Vec<ColumnSchema> {
    alloc::vec![
        ColumnSchema::new("rngtypid", DataType::BigInt, false),
        ColumnSchema::new("rngsubtype", DataType::BigInt, false),
        ColumnSchema::new("rngmultitypid", DataType::BigInt, false),
        ColumnSchema::new("rngcollation", DataType::BigInt, false),
        ColumnSchema::new("rngsubopc", DataType::BigInt, false),
        ColumnSchema::new("rngcanonical", DataType::BigInt, false),
        ColumnSchema::new("rngsubdiff", DataType::BigInt, false),
    ]
}

pub(crate) fn synth_pg_range() -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_range_schema();
    // (range oid, subtype oid) — PG's own numbers, the ones pg_type
    // already publishes here.
    const RANGES: &[(i64, i64)] = &[
        (3904, 23),   // int4range  -> int4
        (3926, 20),   // int8range  -> int8
        (3906, 1700), // numrange   -> numeric
        (3908, 1114), // tsrange    -> timestamp
        (3910, 1184), // tstzrange  -> timestamptz
        (3912, 1082), // daterange  -> date
    ];
    let rows = RANGES
        .iter()
        .map(|(rng, sub)| {
            Row::new(alloc::vec![
                Value::BigInt(*rng),
                Value::BigInt(*sub),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::BigInt(0),
                Value::BigInt(0),
            ])
        })
        .collect();
    (schema, rows)
}

/// v7.39 (round 546) — `pg_catalog.pg_partitioned_table`: one row per
/// partition PARENT.
///
/// SPG has declarative partitioning, so this is real. partstrat is
/// PG's single char — 'r' range, 'l' list, 'h' hash — and partattrs is
/// the int2vector of key positions, 1-based as PG's attnums are.
/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows.
pub(crate) fn pg_partitioned_table_schema() -> Vec<ColumnSchema> {
    alloc::vec![
        ColumnSchema::new("partrelid", DataType::BigInt, false),
        ColumnSchema::new("partstrat", DataType::Text, false),
        ColumnSchema::new("partnatts", DataType::SmallInt, false),
        ColumnSchema::new("partdefid", DataType::BigInt, false),
        ColumnSchema::new("partattrs", DataType::Text, false),
        ColumnSchema::new("partclass", DataType::BigIntArray, false),
        ColumnSchema::new("partcollation", DataType::Text, false),
        ColumnSchema::new("partexprs", DataType::Text, true),
    ]
}

pub(crate) fn synth_pg_partitioned_table(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    use spg_storage::{PartitionKind, PartitionRole};
    let schema = pg_partitioned_table_schema();
    let mut rows: Vec<Row<'static>> = Vec::new();
    for tname in cat.visible_relation_keys() {
        // 9.0.0 (C9) — the loop walks KEYS; `tname` is the name a client
        // reads and `nsp` the schema column it belongs in, while every
        // catalog lookup below goes on `key`.
        let key = tname;
        let nsp = alloc::string::String::from(cat.listed_schema(&key));
        let Some(tname) = cat.listed_name(&key).map(alloc::string::String::from) else {
            continue;
        };
        let Some(t) = cat.get(&key) else { continue };
        let Some(PartitionRole::Parent {
            kind,
            key_column_positions,
            ..
        }) = &t.schema().partition_role
        else {
            continue;
        };
        let Some(oid) = relation_oid(cat, &tname) else {
            continue;
        };
        let strat = match kind {
            PartitionKind::Range => "r",
            PartitionKind::List => "l",
            PartitionKind::Hash => "h",
        };
        let attrs = key_column_positions
            .iter()
            .map(|p| alloc::format!("{}", p + 1))
            .collect::<Vec<_>>()
            .join(" ");
        let zeros = key_column_positions
            .iter()
            .map(|_| alloc::string::String::from("0"))
            .collect::<Vec<_>>()
            .join(" ");
        // 7.38.1 S5.1 — partclass is a REAL oid array now (pg_dump
        // probes `<opclass oid> = ANY(partclass)`); default opclass
        // everywhere = zeros, matching the old rendered form.
        let partclass_arr: Value<'static> =
            Value::BigIntArray(key_column_positions.iter().map(|_| Some(0i64)).collect());
        rows.push(Row::new(alloc::vec![
            Value::BigInt(oid),
            Value::text(strat),
            Value::SmallInt(i16::try_from(key_column_positions.len()).unwrap_or(1)),
            Value::BigInt(0), // partdefid — no DEFAULT partition recorded
            Value::text(attrs),
            partclass_arr,
            Value::text(zeros),
            Value::Null, // partexprs — SPG partitions on columns, not expressions
        ]));
    }
    (schema, rows)
}

/// v7.39 (round 544) — `pg_catalog.pg_cast`, empty, and why.
///
/// PG's pg_cast is a REGISTRY, not a description of what converts. It
/// lists `bool → text` (a registered function) and does NOT list
/// `int4 → text`, which PG resolves through the type's I/O functions
/// without a row. So its content is not derivable from behaviour.
///
/// SPG has no registry at all: `cast_value` is one dispatch on the
/// target type, and there is no CREATE CAST to add to. Listing none is
/// the accurate answer to the question this catalog asks, and it lets a
/// tool reading it conclude "no user-defined casts here" rather than
/// stop, which is what pg_dump did.
///
/// This round DID try deriving the rows by probing the real cast
/// function, and threw the result away: measured against PG18 over the
/// thirty-seven types SPG catalogues, the probe reported 180 pairs PG
/// does not list (the I/O family) and missed 31 that it does — some of
/// those only because the sample value chosen for a type could not
/// convert, which makes a working cast look absent. A catalog built on
/// a heuristic is not a catalog. What the comparison DID find — the
/// conversions PG performs and SPG refused — is fixed in eval/cast.rs
/// this round.
pub(crate) fn synth_pg_cast() -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("castsource", DataType::Oid, false),
        ColumnSchema::new("casttarget", DataType::Oid, false),
        ColumnSchema::new("castfunc", DataType::Oid, false),
        ColumnSchema::new("castcontext", DataType::Char1, false),
        ColumnSchema::new("castmethod", DataType::Char1, false),
    ];
    // v7.39 (round 635, F18) — the casts SPG performs, with PG's context
    // and method for each.
    //
    // The table was not written from PG's source: it is PG's registered
    // casts restricted to the 33 base types SPG has, and rounds 633 and 634
    // then PROBED every one of them against the engine, closing the 23 it
    // could not do. So each row here is a conversion the engine was
    // measured to perform, which is why the catalog can be published at all
    // — an earlier cut was held back rather than claim conversions the
    // engine refuses.
    //
    // `castsource` / `casttarget` are PG's own type oids, which is what
    // SPG's pg_type already reports, so a join against it resolves.
    // `castfunc` is 0 throughout: PG names an implementation function per
    // row and SPG has no pg_proc entry for one, and a client reads the
    // context and the method rather than the function. Recorded as a
    // divergence rather than invented.
    const CASTS: &[(i64, i64, &str, &str)] = &[
        (16, 23, "e", "f"),     // bool -> int4
        (16, 25, "a", "f"),     // bool -> text
        (16, 1042, "a", "f"),   // bool -> bpchar
        (16, 1043, "a", "f"),   // bool -> varchar
        (17, 20, "e", "f"),     // bytea -> int8
        (17, 21, "e", "f"),     // bytea -> int2
        (17, 23, "e", "f"),     // bytea -> int4
        (18, 23, "e", "f"),     // char -> int4
        (18, 25, "i", "f"),     // char -> text
        (18, 1042, "a", "f"),   // char -> bpchar
        (18, 1043, "a", "f"),   // char -> varchar
        (19, 25, "i", "f"),     // name -> text
        (19, 1042, "a", "f"),   // name -> bpchar
        (19, 1043, "a", "f"),   // name -> varchar
        (20, 17, "e", "f"),     // int8 -> bytea
        (20, 21, "a", "f"),     // int8 -> int2
        (20, 23, "a", "f"),     // int8 -> int4
        (20, 24, "i", "f"),     // int8 -> regproc
        (20, 26, "i", "f"),     // int8 -> oid
        (20, 700, "i", "f"),    // int8 -> float4
        (20, 701, "i", "f"),    // int8 -> float8
        (20, 790, "a", "f"),    // int8 -> money
        (20, 1560, "e", "f"),   // int8 -> bit
        (20, 1700, "i", "f"),   // int8 -> numeric
        (21, 17, "e", "f"),     // int2 -> bytea
        (21, 20, "i", "f"),     // int2 -> int8
        (21, 23, "i", "f"),     // int2 -> int4
        (21, 24, "i", "f"),     // int2 -> regproc
        (21, 26, "i", "f"),     // int2 -> oid
        (21, 700, "i", "f"),    // int2 -> float4
        (21, 701, "i", "f"),    // int2 -> float8
        (21, 1700, "i", "f"),   // int2 -> numeric
        (23, 16, "e", "f"),     // int4 -> bool
        (23, 17, "e", "f"),     // int4 -> bytea
        (23, 18, "e", "f"),     // int4 -> char
        (23, 20, "i", "f"),     // int4 -> int8
        (23, 21, "a", "f"),     // int4 -> int2
        (23, 24, "i", "b"),     // int4 -> regproc
        (23, 26, "i", "b"),     // int4 -> oid
        (23, 700, "i", "f"),    // int4 -> float4
        (23, 701, "i", "f"),    // int4 -> float8
        (23, 790, "a", "f"),    // int4 -> money
        (23, 1560, "e", "f"),   // int4 -> bit
        (23, 1700, "i", "f"),   // int4 -> numeric
        (24, 20, "a", "f"),     // regproc -> int8
        (24, 23, "a", "b"),     // regproc -> int4
        (24, 26, "i", "b"),     // regproc -> oid
        (25, 18, "a", "f"),     // text -> char
        (25, 19, "i", "f"),     // text -> name
        (25, 142, "e", "f"),    // text -> xml
        (25, 1042, "i", "b"),   // text -> bpchar
        (25, 1043, "i", "b"),   // text -> varchar
        (26, 20, "a", "f"),     // oid -> int8
        (26, 23, "a", "b"),     // oid -> int4
        (26, 24, "i", "b"),     // oid -> regproc
        (114, 3802, "a", "i"),  // json -> jsonb
        (142, 25, "a", "b"),    // xml -> text
        (142, 1042, "a", "b"),  // xml -> bpchar
        (142, 1043, "a", "b"),  // xml -> varchar
        (650, 25, "a", "f"),    // cidr -> text
        (650, 869, "i", "b"),   // cidr -> inet
        (650, 1042, "a", "f"),  // cidr -> bpchar
        (650, 1043, "a", "f"),  // cidr -> varchar
        (700, 20, "a", "f"),    // float4 -> int8
        (700, 21, "a", "f"),    // float4 -> int2
        (700, 23, "a", "f"),    // float4 -> int4
        (700, 701, "i", "f"),   // float4 -> float8
        (700, 1700, "a", "f"),  // float4 -> numeric
        (701, 20, "a", "f"),    // float8 -> int8
        (701, 21, "a", "f"),    // float8 -> int2
        (701, 23, "a", "f"),    // float8 -> int4
        (701, 700, "a", "f"),   // float8 -> float4
        (701, 1700, "a", "f"),  // float8 -> numeric
        (790, 1700, "a", "f"),  // money -> numeric
        (869, 25, "a", "f"),    // inet -> text
        (869, 650, "a", "f"),   // inet -> cidr
        (869, 1042, "a", "f"),  // inet -> bpchar
        (869, 1043, "a", "f"),  // inet -> varchar
        (1042, 18, "a", "f"),   // bpchar -> char
        (1042, 19, "i", "f"),   // bpchar -> name
        (1042, 25, "i", "f"),   // bpchar -> text
        (1042, 142, "e", "f"),  // bpchar -> xml
        (1042, 1042, "i", "f"), // bpchar -> bpchar
        (1042, 1043, "i", "f"), // bpchar -> varchar
        (1043, 18, "a", "f"),   // varchar -> char
        (1043, 19, "i", "f"),   // varchar -> name
        (1043, 25, "i", "b"),   // varchar -> text
        (1043, 142, "e", "f"),  // varchar -> xml
        (1043, 1042, "i", "b"), // varchar -> bpchar
        (1043, 1043, "i", "f"), // varchar -> varchar
        (1082, 1114, "i", "f"), // date -> timestamp
        (1082, 1184, "i", "f"), // date -> timestamptz
        (1083, 1083, "i", "f"), // time -> time
        (1083, 1186, "i", "f"), // time -> interval
        (1083, 1266, "i", "f"), // time -> timetz
        (1114, 1082, "a", "f"), // timestamp -> date
        (1114, 1083, "a", "f"), // timestamp -> time
        (1114, 1114, "i", "f"), // timestamp -> timestamp
        (1114, 1184, "i", "f"), // timestamp -> timestamptz
        (1184, 1082, "a", "f"), // timestamptz -> date
        (1184, 1083, "a", "f"), // timestamptz -> time
        (1184, 1114, "a", "f"), // timestamptz -> timestamp
        (1184, 1184, "i", "f"), // timestamptz -> timestamptz
        (1184, 1266, "a", "f"), // timestamptz -> timetz
        (1186, 1083, "a", "f"), // interval -> time
        (1186, 1186, "i", "f"), // interval -> interval
        (1266, 1083, "a", "f"), // timetz -> time
        (1266, 1266, "i", "f"), // timetz -> timetz
        (1560, 20, "e", "f"),   // bit -> int8
        (1560, 23, "e", "f"),   // bit -> int4
        (1560, 1560, "i", "f"), // bit -> bit
        (1560, 1562, "i", "b"), // bit -> varbit
        (1562, 1560, "i", "b"), // varbit -> bit
        (1562, 1562, "i", "f"), // varbit -> varbit
        (1700, 20, "a", "f"),   // numeric -> int8
        (1700, 21, "a", "f"),   // numeric -> int2
        (1700, 23, "a", "f"),   // numeric -> int4
        (1700, 700, "i", "f"),  // numeric -> float4
        (1700, 701, "i", "f"),  // numeric -> float8
        (1700, 790, "a", "f"),  // numeric -> money
        (1700, 1700, "i", "f"), // numeric -> numeric
        (3802, 16, "e", "f"),   // jsonb -> bool
        (3802, 20, "e", "f"),   // jsonb -> int8
        (3802, 21, "e", "f"),   // jsonb -> int2
        (3802, 23, "e", "f"),   // jsonb -> int4
        (3802, 114, "a", "i"),  // jsonb -> json
        (3802, 700, "e", "f"),  // jsonb -> float4
        (3802, 701, "e", "f"),  // jsonb -> float8
        (3802, 1700, "e", "f"), // jsonb -> numeric
    ];
    let mut rows: Vec<Row<'static>> = Vec::with_capacity(CASTS.len());
    for (i, (src, tgt, ctx, meth)) in CASTS.iter().enumerate() {
        rows.push(Row::new(alloc::vec![
            Value::BigInt(OID_CAST_BASE + i as i64),
            Value::BigInt(*src),
            Value::BigInt(*tgt),
            Value::BigInt(0),
            char1(ctx),  // castcontext
            char1(meth), // castmethod
        ]));
    }
    (schema, rows)
}

/// v7.39 (round 541) — `pg_catalog.pg_foreign_table`, empty.
///
/// `pg_dump` reads it for every relation of kind 'f':
///
/// ```text
///     CASE WHEN c.relkind = 'f'
///          THEN (SELECT ftserver FROM pg_catalog.pg_foreign_table
///                WHERE ftrelid = c.oid)
///          ELSE 0 END AS foreignserver
/// ```
///
/// SPG has no foreign tables, so the answer is no rows — which is also
/// what PG reports on a database that has none. A catalog that exists
/// and is empty and a catalog that does not exist are different things
/// to a tool: the second stops it.
/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows. The rows and the
/// columns come from one place, so `pg_attribute` cannot describe
/// a relation differently from how it answers.
pub(crate) fn pg_foreign_table_schema() -> Vec<ColumnSchema> {
    alloc::vec![
        ColumnSchema::new("ftrelid", DataType::BigInt, false),
        ColumnSchema::new("ftserver", DataType::BigInt, false),
        ColumnSchema::new("ftoptions", DataType::TextArray, true),
    ]
}

pub(crate) fn synth_pg_foreign_table() -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_foreign_table_schema();
    (schema, Vec::new())
}

/// Synthesise `pg_catalog.pg_extension`. SPG ships its "extension"
/// v7.39 (round 697) — the extensions this build provides, and the single
/// place that says so.
///
/// `pg_extension` read it from a local literal and `CREATE EXTENSION` /
/// `DROP EXTENSION` read nothing at all, so `CREATE EXTENSION nosuch`
/// reported plain success and `pg_extension` then did not list it.
///
/// PG18 ERRORS on a name it does not have (`is not available`). SPG does
/// not, and the reason is the zero-customer-change line rather than
/// laziness: a customer's dump carries `CREATE EXTENSION pgcrypto`, PG
/// restores it because the extension can be installed there, and SPG cannot
/// be installed into. Refusing would turn a restore that works today into
/// one that needs the dump edited. It warns instead — the same resolution
/// F36 reached for a collation this build cannot perform: record it, say
/// plainly what is not provided, do not pretend.
///
/// `pgcrypto` is on the list because SPG really does answer it — `digest`
/// and `gen_random_uuid` both work, measured. `hstore` is not, and does
/// not: `'a=>1'::hstore` says the type does not exist.
pub(crate) const INSTALLED_EXTENSIONS: &[(&str, &str)] = &[
    ("plpgsql", "1.0"),
    ("vector", "0.8.0"),
    ("pg_trgm", "1.6"),
    // 8.0.3 — 1.4 is what PG 18.6 installs, measured.
    ("pgcrypto", "1.4"),
    // v7.39 (round 780, F31-D1) — hstore's type, codec and both text
    // conversions are first-class since v7.17.0; once the type-NAME
    // map listed it (same round) every hstore spelling works, so the
    // install is real rather than a warned no-op.
    ("hstore", "1.8"),
    // 9.0.0 — the `uuid_generate_*` family is real here, so the install
    // is real too. It used to WARN that nothing the extension supplies
    // would be available, while supplying it.
    ("uuid-ossp", "1.1"),
];

/// surfaces natively (vector, pg_trgm, plpgsql-shaped DO blocks), so
/// the table lists those as installed — `SELECT … FROM pg_extension
/// WHERE extname = 'vector'` probes from PG clients (mailrs embed
/// round-12) answer truthfully about capability presence.
pub(crate) fn synth_pg_extension(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    // v7.39 (round 539) — PG18's full eight columns, in its order.
    //
    // Four were missing and `extnamespace` was the schema's NAME where
    // PG has its OID, which is what `pg_dump` joins on — so its very
    // first catalog query failed and no dump ran at all:
    //
    //   SELECT x.tableoid, x.oid, x.extname, n.nspname, x.extrelocatable,
    //          x.extversion, x.extconfig, x.extcondition
    //   FROM pg_extension x JOIN pg_namespace n ON n.oid = x.extnamespace
    //
    // Values measured on PG18 for an installed extension: owner 10,
    // namespace 11 (pg_catalog), not relocatable, and NULL for both
    // array columns.
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("extname", DataType::Name, false),
        ColumnSchema::new("extowner", DataType::Oid, false),
        ColumnSchema::new("extnamespace", DataType::Oid, false),
        ColumnSchema::new("extrelocatable", DataType::Bool, false),
        ColumnSchema::new("extversion", DataType::Text, false),
        ColumnSchema::new("extconfig", DataType::OidArray, true),
        ColumnSchema::new("extcondition", DataType::TextArray, true),
    ];
    // 8.0.3 — what this database INSTALLED, not what the build provides:
    // `plpgsql`, which every PostgreSQL database has (oid 13643, in
    // pg_catalog, not relocatable — measured on 18.6, and below 16384, so
    // `pg_dump` treats it as built in), and each `CREATE EXTENSION`, in
    // the schema it named. The ones SPG provides are relocatable, as
    // PostgreSQL's are.
    let version_of = |name: &str| {
        INSTALLED_EXTENSIONS
            .iter()
            .find(|(e, _)| e.eq_ignore_ascii_case(name))
            .map_or("", |(_, v)| v)
    };
    let row = |oid: i64, name: &str, namespace: i64, relocatable: bool| {
        Row::new(alloc::vec![
            Value::BigInt(oid),
            Value::text(String::from(name)),
            Value::BigInt(crate::role_directory::BOOTSTRAP_OID),
            Value::BigInt(namespace),
            Value::Bool(relocatable),
            Value::text(String::from(version_of(name))),
            Value::Null,
            Value::Null,
        ])
    };
    let mut rows: Vec<Row<'static>> =
        alloc::vec![row(13643, crate::extension::ALWAYS_INSTALLED, 11, false)];
    for (i, (name, schema)) in cat.extensions().iter().enumerate() {
        let namespace = match schema.as_str() {
            "pg_catalog" => 11,
            "information_schema" => 13000,
            _ => 2200,
        };
        rows.push(row(OID_EXTENSION_BASE + i as i64, name, namespace, true));
    }
    (schema, rows)
}

/// 8.0.3 — `(oid, name)` of every installed extension, in `pg_extension`'s
/// numbering.
fn extension_oids(cat: &Catalog) -> Vec<(i64, String)> {
    let mut out = alloc::vec![(13643, String::from(crate::extension::ALWAYS_INSTALLED))];
    out.extend(
        cat.extensions()
            .keys()
            .enumerate()
            .map(|(i, name)| (OID_EXTENSION_BASE + i as i64, name.clone())),
    );
    out
}

/// 8.0.3 — the comment each extension's control file gives it, measured on
/// PG 18.6 (`pg_available_extensions.comment`). An extension not listed
/// here gets no comment rather than an invented one.
const EXTENSION_COMMENTS: &[(&str, &str)] = &[
    ("citext", "data type for case-insensitive character strings"),
    ("hstore", "data type for storing sets of (key, value) pairs"),
    (
        "pg_stat_statements",
        "track planning and execution statistics of all SQL statements executed",
    ),
    (
        "pg_trgm",
        "text similarity measurement and index searching based on trigrams",
    ),
    ("pgcrypto", "cryptographic functions"),
    ("plpgsql", "PL/pgSQL procedural language"),
    (
        "uuid-ossp",
        "generate universally unique identifiers (UUIDs)",
    ),
];

/// 8.0.3 — the oids of installed extensions, a band of their own above
/// FirstNormalObjectId (16384), so `pg_dump` dumps them.
const OID_EXTENSION_BASE: i64 = 700_000;

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
    let args = function_argument_list(f);
    let returns = function_result_type(f);
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

/// 8.0.3 — `pg_get_constraintdef` of a domain constraint: `NOT NULL`, or
/// `CHECK (<predicate>)` in catalog form with `VALUE` typed as the domain's
/// base type (`CHECK ((VALUE <> ''::text))`, measured on 18.6).
pub(crate) fn domain_constraint_def(
    cat: &Catalog,
    typid: i64,
    conname: &str,
    qualify: bool,
) -> Option<alloc::string::String> {
    let (_, _, domains) = user_type_oids(cat);
    let (name, _) = domains.into_iter().find(|(_, o)| *o == typid)?;
    let d = cat.domain_types().get(&name)?;
    if conname == alloc::format!("{name}_not_null") && !d.nullable {
        return Some(alloc::string::String::from("NOT NULL"));
    }
    let check = d.checks.iter().find(|c| c.name == conname)?;
    let body =
        crate::catalog_deparse::domain_check_text(&check.expr, d.base_type, qualify.then_some(cat))
            .unwrap_or_else(|| alloc::format!("({})", check.expr));
    Some(alloc::format!("CHECK ({body})"))
}

/// 8.0.3 — `pg_get_triggerdef`: the CREATE TRIGGER PG prints, measured on
/// 18.6. Events in PG's fixed order (INSERT, DELETE, UPDATE, TRUNCATE)
/// whatever order they were written in; the table always schema-qualified;
/// the function qualified when the search path leaves `public` out; the
/// WHEN predicate in catalog form, doubly parenthesised.
pub(crate) fn trigger_def(cat: &Catalog, oid: i64, qualify: bool) -> Option<alloc::string::String> {
    let index = usize::try_from(oid.checked_sub(OID_TRIGGER_BASE + 1)?).ok()?;
    let t = cat.triggers().get(index)?;
    let has = |ev: &str| t.events.iter().any(|e| e.eq_ignore_ascii_case(ev));
    let mut events: Vec<alloc::string::String> = Vec::new();
    for ev in ["INSERT", "DELETE", "UPDATE", "TRUNCATE"] {
        if !has(ev) {
            continue;
        }
        if ev == "UPDATE" && !t.update_columns.is_empty() {
            events.push(alloc::format!("UPDATE OF {}", t.update_columns.join(", ")));
        } else {
            events.push(alloc::string::String::from(ev));
        }
    }
    let level = if t.for_each.eq_ignore_ascii_case("ROW") {
        "ROW"
    } else {
        "STATEMENT"
    };
    let when = if t.when_condition.trim().is_empty() {
        alloc::string::String::new()
    } else {
        let cols = cat
            .get(&t.table)
            .map(|tb| tb.schema().columns.clone())
            .unwrap_or_default();
        let pred = crate::catalog_deparse::predicate_text(
            &t.when_condition,
            &cols,
            qualify.then_some(cat),
        )
        .unwrap_or_else(|| t.when_condition.clone());
        alloc::format!(" WHEN ({pred})")
    };
    Some(alloc::format!(
        "CREATE TRIGGER {} {} {} ON public.{} FOR EACH {level}{when} EXECUTE FUNCTION {}()",
        t.name,
        t.timing.to_ascii_uppercase(),
        events.join(" OR "),
        t.table,
        crate::qualify::user_object_name(&t.function, qualify),
    ))
}

/// 8.0.3 — a user function's argument list as PG prints it
/// (`pg_get_function_arguments`).
pub(crate) fn function_argument_list(f: &spg_storage::FunctionDef) -> alloc::string::String {
    canonical_arg_list(&f.args_repr)
}

/// 8.0.3 — a user function's result type as PG prints it
/// (`pg_get_function_result`).
pub(crate) fn function_result_type(f: &spg_storage::FunctionDef) -> alloc::string::String {
    canonical_type_word(f.returns.trim())
}

/// 8.0.3 — the user function behind a `pg_proc` oid, in the numbering
/// `synth_pg_proc` publishes: `OID_FUNC_BASE + 1` for the first.
pub(crate) fn user_function_for_oid(cat: &Catalog, oid: i64) -> Option<&spg_storage::FunctionDef> {
    let index = usize::try_from(oid.checked_sub(OID_FUNC_BASE + 1)?).ok()?;
    cat.functions().values().nth(index)
}

/// v7.39 (round 322, V46) — the declared attribute words in PG's print
/// order. Shared by `pg_get_functiondef`; empty when everything is at its
/// default.
pub(crate) fn function_attr_words(
    f: &spg_storage::FunctionDef,
) -> alloc::vec::Vec<alloc::string::String> {
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
    let inner = args_repr
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')');
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
/// v7.39 (round 543) — the parameter NAMES out of the stored display
/// form, for `pg_proc.proargnames`. NULL when the function takes none,
/// as PG's is.
fn declared_arg_names(args_repr: &str) -> Value<'static> {
    let inner = args_repr
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')');
    if inner.trim().is_empty() {
        return Value::Null;
    }
    let names: Vec<Option<alloc::string::String>> = inner
        .split(',')
        .map(|part| {
            let part = part.trim();
            part.split_once(char::is_whitespace)
                .map(|(n, _)| alloc::string::String::from(n))
        })
        .collect();
    if names.iter().any(Option::is_none) {
        // A positional signature (`(integer, text)`) names nothing.
        return Value::Null;
    }
    Value::TextArray(names)
}

pub(crate) fn canonical_arg_types(args_repr: &str) -> alloc::string::String {
    let inner = args_repr
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')');
    if inner.trim().is_empty() {
        return alloc::string::String::new();
    }
    inner
        .split(',')
        .map(|part| {
            let part = part.trim();
            let ty = part
                .split_once(char::is_whitespace)
                .map_or(part, |(_, t)| t);
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
            alloc::format!("{head}\n  VALUES {}", &rendered[vpos + " VALUES ".len()..])
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
/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows. The rows and the
/// columns come from one place, so `pg_attribute` cannot describe
/// a relation differently from how it answers.
pub(crate) fn pg_rewrite_schema() -> Vec<ColumnSchema> {
    alloc::vec![
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("rulename", DataType::Text, false),
        ColumnSchema::new("ev_class", DataType::BigInt, false),
        ColumnSchema::new("ev_type", DataType::Text, false),
        ColumnSchema::new("ev_enabled", DataType::Text, false),
        ColumnSchema::new("is_instead", DataType::Bool, false),
        ColumnSchema::new("ev_qual", DataType::Text, true),
        ColumnSchema::new("ev_action", DataType::Text, true),
    ]
}

pub(crate) fn synth_pg_rewrite(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_rewrite_schema();
    // `ev_class` has to be the SAME oid pg_class / pg_constraint hand out
    // for that table, or a join against them silently returns nothing.
    let mut names: Vec<String> = cat.visible_relation_keys();
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
        ColumnSchema::new("schemaname", DataType::Name, false),
        ColumnSchema::new("tablename", DataType::Name, false),
        ColumnSchema::new("rulename", DataType::Name, false),
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

pub(crate) fn synth_pg_views(
    cat: &Catalog,
    roles: &crate::role_directory::RoleDirectory,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("schemaname", DataType::Name, false),
        ColumnSchema::new("viewname", DataType::Name, false),
        // v7.39 (round 542) — PG's third column, which SPG omitted.
        ColumnSchema::new("viewowner", DataType::Name, false),
        ColumnSchema::new("definition", DataType::Text, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for (stored, def) in cat.views_all() {
        let Some(name) = cat.listed_name(stored) else {
            continue;
        };
        let owner = cat.object_owner(spg_storage::NonTableKind::View, stored);
        rows.push(Row::new(alloc::vec![
            Value::text("public"),
            Value::text(name.to_string()),
            Value::text(String::from(roles.owner_name(owner))),
            Value::text(def.body.clone()),
        ]));
    }
    (schema, rows)
}

/// 8.0.3 — a sequence's owner. One made for a serial column records none
/// and belongs to its table's owner, which is who ran the CREATE TABLE that
/// made it.
fn sequence_owner<'a>(cat: &'a Catalog, def: &'a spg_storage::SequenceDef) -> Option<&'a str> {
    def.owner.as_deref().or_else(|| {
        def.owned_by
            .as_ref()
            .and_then(|(table, _)| cat.get(table))
            .and_then(|t| t.schema().owner.as_deref())
    })
}

/// v7.39 (round 542) — `pg_catalog.pg_matviews`, with rows.
///
/// It shared pg_views' SCHEMA and was pinned empty, with the note "SPG
/// has no materialised view surface yet". That stopped being true in
/// round 338, when pg_class began reporting relkind 'm' — so a tool
/// listing materialized views the canonical way found none of them, and
/// its column was `viewname` where PG's is `matviewname`.
pub(crate) fn synth_pg_matviews(
    cat: &Catalog,
    roles: &crate::role_directory::RoleDirectory,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("schemaname", DataType::Name, false),
        ColumnSchema::new("matviewname", DataType::Name, false),
        ColumnSchema::new("matviewowner", DataType::Name, false),
        ColumnSchema::new("tablespace", DataType::Name, true),
        ColumnSchema::new("hasindexes", DataType::Bool, false),
        ColumnSchema::new("ispopulated", DataType::Bool, false),
        ColumnSchema::new("definition", DataType::Text, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for stored in cat.table_names() {
        let Some(name) = cat.listed_name(&stored).map(alloc::string::String::from) else {
            continue;
        };
        let Some(t) = cat.get(&stored) else { continue };
        let Some(body) = cat.materialized_views().get(&name) else {
            continue;
        };
        rows.push(Row::new(alloc::vec![
            Value::text("public"),
            Value::text(name.clone()),
            Value::text(String::from(roles.owner_name(t.schema().owner.as_deref()))),
            Value::Null,
            Value::Bool(!t.indices().is_empty()),
            Value::Bool(true),
            Value::text(body.clone()),
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
/// v7.40.11 — the SHAPE `SHOW <name>` answers with, read by the
/// executor and by Describe.
///
/// Describe answered `Vec::new()` for every statement that is not a
/// SELECT or a DML with RETURNING, and Execute then emitted DataRows
/// for it. That is a protocol violation with a name — psql prints
/// `server sent data ("D" message) without prior row description
/// ("T" message)` — and sqlx surfaces it as a row with zero columns.
/// `SHOW TimeZone` is how a pooled client reads back the zone it asked
/// for, so it was the first thing the reporter tried and could not run.
///
/// The column name is the GUC's CANONICAL spelling. PG answers a column
/// called `TimeZone` for `SHOW timezone`, `SHOW TimeZone` and
/// `SHOW TIMEZONE` alike; SPG lower-cased whatever arrived, so a client
/// selecting that column BY NAME got nothing.
pub(crate) fn show_parameter_columns(name: &str) -> Vec<ColumnSchema> {
    if name.eq_ignore_ascii_case("all") {
        return alloc::vec![
            ColumnSchema::new("name", DataType::Text, false),
            ColumnSchema::new("setting", DataType::Text, false),
            ColumnSchema::new("description", DataType::Text, false),
        ];
    }
    // MySQL's diagnostics-area forms, which carry their own headings.
    if name.eq_ignore_ascii_case("warnings") {
        return alloc::vec![
            ColumnSchema::new("Level", DataType::Text, false),
            ColumnSchema::new("Code", DataType::Int, false),
            ColumnSchema::new("Message", DataType::Text, false),
        ];
    }
    if name.eq_ignore_ascii_case("count(*) warnings") {
        return alloc::vec![ColumnSchema::new(
            "@@session.warning_count",
            DataType::BigInt,
            false
        )];
    }
    alloc::vec![ColumnSchema::new(
        canonical_guc_name(name),
        DataType::Text,
        false
    )]
}

/// The spelling PostgreSQL prints for this parameter — its own, when
/// the inventory knows it; otherwise what the caller wrote.
pub(crate) fn canonical_guc_name(name: &str) -> alloc::string::String {
    canonical_gucs()
        .iter()
        .find(|(n, ..)| n.eq_ignore_ascii_case(name))
        .map_or_else(
            || alloc::string::String::from(name),
            |(n, ..)| alloc::string::String::from(*n),
        )
}

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
            crate::PG_SERVER_VERSION,
            "Preset Options",
            "string",
            "internal",
        ),
        (
            "server_version_num",
            crate::PG_SERVER_VERSION_NUM,
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
/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows. The rows and the
/// columns come from one place, so `pg_attribute` cannot describe
/// a relation differently from how it answers.
pub(crate) fn pg_timezone_names_schema() -> Vec<ColumnSchema> {
    alloc::vec![
        ColumnSchema::new("name", DataType::Text, false),
        ColumnSchema::new("abbrev", DataType::Text, false),
        ColumnSchema::new("utc_offset", DataType::Interval, false),
        ColumnSchema::new("is_dst", DataType::Bool, false),
    ]
}

pub(crate) fn synth_pg_timezone_names(engine: &Engine) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_timezone_names_schema();
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
                    kind: spg_storage::IntervalKind::Finite,
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
/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows. The rows and the
/// columns come from one place, so `pg_attribute` cannot describe
/// a relation differently from how it answers.
pub(crate) fn pg_timezone_abbrevs_schema() -> Vec<ColumnSchema> {
    alloc::vec![
        ColumnSchema::new("abbrev", DataType::Text, false),
        ColumnSchema::new("utc_offset", DataType::Interval, false),
        ColumnSchema::new("is_dst", DataType::Bool, false),
    ]
}

pub(crate) fn synth_pg_timezone_abbrevs(engine: &Engine) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = pg_timezone_abbrevs_schema();
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
                    kind: spg_storage::IntervalKind::Finite,
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
/// v7.38.18 (C5) — one `pg_settings` row for a parameter whose metadata
/// came from PG 18.4 rather than from SPG's own list. Its boot value is
/// already in PG's raw reporting form and its unit is PG's, so it skips
/// the human-form conversion the canonical rows go through.
fn pg_guc_row(
    name: &str,
    boot: &str,
    cat: &str,
    vartype: &str,
    context: &str,
    unit: &str,
    desc: &str,
    overridden: Option<String>,
) -> Row<'static> {
    let source = if overridden.is_some() {
        "session"
    } else {
        "default"
    };
    let setting = overridden.unwrap_or_else(|| boot.into());
    let unit = if unit.is_empty() {
        Value::Null
    } else {
        Value::text::<String>(unit.into())
    };
    Row::new(alloc::vec![
        Value::text::<String>(name.into()),
        Value::text(setting),
        unit,
        Value::text::<String>(cat.into()),
        Value::text::<String>(desc.into()),
        Value::Null, // extra_desc
        Value::text::<String>(context.into()),
        Value::text::<String>(vartype.into()),
        Value::text::<String>(source.into()),
        Value::Null, // min_val
        Value::Null, // max_val
        Value::Null, // enumvals
        Value::text::<String>(boot.into()),
        Value::text::<String>(boot.into()), // reset_val = boot_val
        Value::Null,                        // sourcefile
        Value::Null,                        // sourceline
        Value::Bool(false),                 // pending_restart
    ])
}

/// 9.0.0 — the shape alone, so the catalog can list this
/// relation's columns without building its rows. See `CATALOG_RELATIONS`.
pub(crate) fn pg_settings_schema() -> Vec<ColumnSchema> {
    alloc::vec![
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
        ColumnSchema::new("enumvals", DataType::TextArray, true),
        ColumnSchema::new("boot_val", DataType::Text, true),
        ColumnSchema::new("reset_val", DataType::Text, true),
        ColumnSchema::new("sourcefile", DataType::Text, true),
        ColumnSchema::new("sourceline", DataType::Int, true),
        ColumnSchema::new("pending_restart", DataType::Bool, false),
    ]
}

pub(crate) fn synth_pg_settings(engine: &Engine) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    // v7.38 (read01 P3.22) — PG 18's full 17-column pg_settings shape so
    // admin tools that filter on context / vartype / source (pgAdmin's
    // parameter editor, postgres_exporter) get the columns they expect.
    let schema = pg_settings_schema();
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
        let unit =
            crate::session::guc_unit(name).map_or(Value::Null, |u| Value::text::<String>(u.into()));
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
            Value::Null,           // sourcefile
            Value::Null,           // sourceline
            Value::Bool(false),    // pending_restart
        ]));
    };
    for &(name, val, cat, vartype, context) in defaults {
        push(name, val, cat, vartype, context);
    }
    // v7.38.18 (C5) — and every other parameter PG 18.4 has. Until now
    // this list stopped at the ones SPG reads, so `pg_settings` held 31
    // rows against PG's 398 and a client that enumerated settings saw a
    // server that looked unconfigured.
    //
    // The row is not a new claim. `SHOW archive_command` already
    // answered `''` and `SET archive_command = 'x'` already answered
    // "cannot be changed now" — only `pg_settings` said the parameter
    // did not exist, which made it the one surface out of three that
    // disagreed. `source` is what separates the two groups: a parameter
    // SPG reads and a session has set reads `session`, everything else
    // reads `default`, exactly as in PG.
    //
    // Metadata (context, boot value, vartype, category, unit) comes from
    // a live PG 18.4; `canonical_gucs` above wins wherever the two
    // overlap, because SPG's own value for `work_mem` is the true one.
    for &(name, context, _human, vartype, cat, unit, boot, desc) in
        crate::guc_catalog::PG_GUC_CONTEXTS
    {
        if defaults
            .iter()
            .any(|(n, ..)| (*n).eq_ignore_ascii_case(name))
        {
            continue;
        }
        let overridden = engine
            .session_params
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone());
        rows.push(pg_guc_row(
            name, boot, cat, vartype, context, unit, desc, overridden,
        ));
    }
    // Session-set params not in the canonical list get their own rows;
    // vartype is inferred from the value and source is always "session".
    for (k, v) in &engine.session_params {
        if defaults.iter().any(|(n, ..)| (*n).eq_ignore_ascii_case(k)) {
            continue;
        }
        // v7.38.18 (C5) — and not a second row for one PG18 knows about
        // either. This loop used to see every name outside SPG's own
        // thirty-one as unknown; now that the PG18 table is reported
        // too, `SET random_page_cost = 3` produced the parameter twice.
        if crate::guc_catalog::guc_context(k).is_some() {
            continue;
        }
        // v7.39 (GUC knife 2) — customised (dotted) parameters are NOT
        // rows of PG's pg_settings (only registered extension GUCs are);
        // they remain readable via current_setting().
        if k.contains('.') {
            continue;
        }
        // v7.40.12 — nor the reserved `__spg_` keys. Their own doc
        // comment says the prefix "keeps it out of the user-visible GUC
        // namespace", and this loop was the place that did not honour
        // it: every wire connection sets `__spg_session_user`, so every
        // wire connection had a `pg_settings` row PG has no name for.
        // It hid a real gap, too — the row count matched PG's 399 while
        // the inventory was one PG name short.
        if k.starts_with("__spg_") {
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
pub(crate) fn synth_pg_tables(
    cat: &Catalog,
    roles: &crate::role_directory::RoleDirectory,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("schemaname", DataType::Name, false),
        ColumnSchema::new("tablename", DataType::Name, false),
        ColumnSchema::new("tableowner", DataType::Name, false),
        ColumnSchema::new("tablespace", DataType::Name, true),
        ColumnSchema::new("hasindexes", DataType::Bool, false),
        ColumnSchema::new("hasrules", DataType::Bool, false),
        ColumnSchema::new("hastriggers", DataType::Bool, false),
        ColumnSchema::new("rowsecurity", DataType::Bool, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for tname in cat.visible_relation_keys() {
        // 9.0.0 (C9) — the loop walks KEYS; `tname` is the name a client
        // reads and `nsp` the schema column it belongs in, while every
        // catalog lookup below goes on `key`.
        let key = tname;
        let nsp = alloc::string::String::from(cat.listed_schema(&key));
        let Some(tname) = cat.listed_name(&key).map(alloc::string::String::from) else {
            continue;
        };
        let Some(t) = cat.get(&key) else { continue };
        let has_indexes = !t.indices().is_empty() || !t.schema().uniqueness_constraints.is_empty();
        rows.push(Row::new(alloc::vec![
            Value::text(nsp.clone()),
            Value::text(tname.clone()),
            Value::text(String::from(roles.owner_name(t.schema().owner.as_deref()))),
            Value::Null,
            Value::Bool(has_indexes),
            Value::Bool(false),
            Value::Bool(cat.triggers().iter().any(|tg| tg.table == key)),
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
    for tname in cat.visible_relation_keys() {
        // 9.0.0 (C9) — the loop walks KEYS; `tname` is the name a client
        // reads and `nsp` the schema column it belongs in, while every
        // catalog lookup below goes on `key`.
        let key = tname;
        let nsp = alloc::string::String::from(cat.listed_schema(&key));
        let Some(tname) = cat.listed_name(&key).map(alloc::string::String::from) else {
            continue;
        };
        let Some(t) = cat.get(&key) else { continue };
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
                    Value::text(nsp.clone()),
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
    for tname in cat.visible_relation_keys() {
        // 9.0.0 (C9) — the loop walks KEYS; `tname` is the name a client
        // reads and `nsp` the schema column it belongs in, while every
        // catalog lookup below goes on `key`.
        let key = tname;
        let nsp = alloc::string::String::from(cat.listed_schema(&key));
        let Some(tname) = cat.listed_name(&key).map(alloc::string::String::from) else {
            continue;
        };
        let Some(t) = cat.get(&key) else { continue };
        // 9.0.0 (N23) — the OWNER's implicit column privileges, which
        // PostgreSQL lists here and this did not: it reported explicit
        // column GRANTs alone, so an un-granted database answered zero
        // rows where PG 18.6 answers one per (column, privilege). The
        // four are the ones PG's column-level set has — measured:
        // INSERT, REFERENCES, SELECT, UPDATE.
        let owner = t
            .schema()
            .owner
            .clone()
            .unwrap_or_else(|| alloc::string::String::from(crate::session::LOGIN_ROLE));
        for col in &t.schema().columns {
            if col.acl.is_empty() {
                for priv_word in ["INSERT", "REFERENCES", "SELECT", "UPDATE"] {
                    rows.push(Row::new(alloc::vec![
                        Value::text(owner.clone()),
                        Value::text(owner.clone()),
                        Value::text(alloc::string::String::from("app")),
                        Value::text(nsp.clone()),
                        Value::text(tname.clone()),
                        Value::text(col.name.clone()),
                        Value::text(alloc::string::String::from(priv_word)),
                        Value::text(alloc::string::String::from("YES")),
                    ]));
                }
            }
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
                        Value::text(nsp.clone()),
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

/// 9.0.0 (N19) — the shape alone, so the catalog can list this
/// relation's columns without building its rows.
pub(crate) fn pg_description_schema() -> Vec<ColumnSchema> {
    alloc::vec![
        ColumnSchema::new("objoid", DataType::Int, false),
        ColumnSchema::new("classoid", DataType::Int, false),
        ColumnSchema::new("objsubid", DataType::Int, false),
        ColumnSchema::new("description", DataType::Text, false),
    ]
}

pub(crate) fn synth_pg_description(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let cols = pg_description_schema();
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
        // 8.0.3 — a function's comment belongs to its pg_proc row (1255)
        // and a type's to its pg_type row (1247). Both read (0, 0), which
        // matches no object, so `pg_dump` dropped every one of them.
        let function_oid = || {
            cat.functions()
                .values()
                .position(|f| f.name == relname)
                .and_then(|i| i32::try_from(OID_FUNC_BASE + 1 + i as i64).ok())
        };
        let type_oid = || {
            let (e, c, d) = user_type_oids(cat);
            e.into_iter()
                .chain(c)
                .chain(d)
                .find(|(n, _)| n == relname)
                .and_then(|(_, o)| i32::try_from(o).ok())
        };
        let (objoid, classoid) = match (kind, builtin_schema_oid(relname)) {
            ("schema", Some(nsp)) => (nsp, 2615),
            ("function", _) => (function_oid().unwrap_or(0), 1255),
            ("type", _) => (type_oid().unwrap_or(0), 1247),
            _ if is_relation => (objoid, 1259),
            _ => (objoid, 0),
        };
        rows.push(Row::new(alloc::vec![
            Value::Int(objoid),
            Value::Int(classoid),
            Value::Int(subid),
            Value::text(text.clone()),
        ]));
    }
    // 8.0.3 — an extension's comment, which PG takes from its control file
    // and `pg_dump` writes back (`COMMENT ON EXTENSION`).
    for (oid, name) in extension_oids(cat) {
        if let Some((_, text)) = EXTENSION_COMMENTS.iter().find(|(n, _)| *n == name) {
            rows.push(Row::new(alloc::vec![
                Value::Int(i32::try_from(oid).unwrap_or(0)),
                Value::Int(3079),
                Value::Int(0),
                Value::text(*text),
            ]));
        }
    }
    // 8.0.3 — the two schema comments every PostgreSQL database starts
    // with (measured on 18.6), unless a COMMENT ON SCHEMA replaced one.
    // Without them `pg_dump` saw `public` as having had its comment
    // removed and wrote `COMMENT ON SCHEMA public IS ''` into every dump.
    for (nsp, name, text) in [
        (11, "pg_catalog", "system catalog schema"),
        (2200, "public", "standard public schema"),
    ] {
        if !cat
            .comments()
            .contains_key(&alloc::format!("schema:{name}"))
        {
            rows.push(Row::new(alloc::vec![
                Value::Int(nsp),
                Value::Int(2615),
                Value::Int(0),
                Value::text(text),
            ]));
        }
    }
    (cols, rows)
}

/// The oids of the schemas every SPG database has.
fn builtin_schema_oid(name: &str) -> Option<i32> {
    match name {
        "pg_catalog" => Some(11),
        "public" => Some(2200),
        "information_schema" => Some(13000),
        _ => None,
    }
}

/// v7.39 (read01 round 83) — the ONE `CREATE [UNIQUE] INDEX …` renderer, PG's
/// `pg_get_indexdef` spelling. Both `pg_indexes.indexdef` (the catalog view) and
/// the `pg_get_indexdef(regclass)` function had their own copy; the function's
/// was the poorer one — it ignored `idx.expression` (so `lower(name)` came back
/// as `name`), ignored the constraint-backing check (so a primary key's index
/// printed `CREATE INDEX` instead of `CREATE UNIQUE INDEX`), and only ever
/// listed column names. One renderer now feeds both.
/// v7.39.13 — the `CREATE INDEX` PostgreSQL prints for a constraint's
/// own index, which SPG synthesises because its storage has none.
/// The definition of an index SPG has no storage object for: a
/// constraint's, and 9.0.0 a partitioned parent's declaration.
fn render_synthesised_indexdef(t: &spg_storage::Table, ci: &CatalogIndex) -> alloc::string::String {
    let cols: alloc::vec::Vec<alloc::string::String> = ci
        .columns
        .iter()
        .map(|p| {
            t.schema()
                .columns
                .get(*p)
                .map_or_else(|| "?".into(), |c| c.name.clone())
        })
        .collect();
    // 9.0.0 — `UNIQUE` and `ONLY` are read off the entry. A constraint's
    // row is always unique and never ONLY, which is why the hardcoded
    // form served until a parent's declaration needed the same renderer.
    alloc::format!(
        "CREATE {}INDEX {} ON {}public.{} USING {} ({})",
        if ci.is_unique { "UNIQUE " } else { "" },
        ci.name,
        if ci.only { "ONLY " } else { "" },
        ci.table,
        am_name_of_oid(ci.am_oid),
        cols.join(", ")
    )
}

/// v7.40.11 — the `CREATE [UNIQUE] INDEX …` for ONE enumerated catalog
/// index, whichever of the two renderers it needs.
///
/// `pg_indexes.indexdef` held this choice inline and `pg_get_indexdef`
/// had no way to reach it: the function resolved an OID by REPLAYING
/// `catalog_indexes`' counting rule over a different table list, so it
/// answered a neighbouring table's index. Both now enumerate once and
/// render from the entry.
///
/// 8.0.3 — `qualify_in` is the catalog when the reader's search path leaves
/// `public` out, so user functions and types in the key or predicate are
/// written qualified (see `qualify`).
pub(crate) fn catalog_indexdef(
    cat: &Catalog,
    t: &spg_storage::Table,
    ci: &CatalogIndex,
    qualify_in: Option<&Catalog>,
) -> alloc::string::String {
    catalog_indexdef_in(cat, t, ci, qualify_in, false)
}

/// [`catalog_indexdef`], in PostgreSQL's pretty form when `pretty` —
/// `pg_get_indexdef(oid, 0, true)`. Measured on 18.6: the schema
/// qualification goes, an operator-expression key loses the outer pair
/// of parentheses, the predicate loses its own, and the expressions are
/// printed by precedence.
///
/// ```text
///   plain   CREATE INDEX i ON public.t USING btree (((a || 'x'::text))) WHERE (b > 0)
///   pretty  CREATE INDEX i ON t USING btree ((a || 'x'::text)) WHERE b > 0
/// ```
pub(crate) fn catalog_indexdef_in(
    cat: &Catalog,
    t: &spg_storage::Table,
    ci: &CatalogIndex,
    qualify_in: Option<&Catalog>,
    pretty: bool,
) -> alloc::string::String {
    if ci.is_storage
        && let Some(idx) = t.indices().iter().find(|i| i.name == ci.name)
    {
        return render_indexdef_in(t, idx, &ci.table, cat, qualify_in.is_some(), pretty);
    }
    render_synthesised_indexdef(t, ci)
}

/// 9.0.0 — one key part of an index as `pg_get_indexdef` prints it: the
/// column or the expression (catalog form, operator expressions doubly
/// parenthesised), then COLLATE, a non-default operator class and the
/// ordering clause. The whole definition and `pg_get_indexdef(oid, n)`
/// both print through this.
pub(crate) fn render_index_part(
    t: &spg_storage::Table,
    idx: &spg_storage::Index,
    cat: &Catalog,
    qualify_in: Option<&Catalog>,
    i: usize,
    attrs_only: bool,
) -> alloc::string::String {
    render_index_part_in(t, idx, cat, qualify_in, i, attrs_only, false)
}

/// [`render_index_part`], in the pretty form when `pretty`.
pub(crate) fn render_index_part_in(
    t: &spg_storage::Table,
    idx: &spg_storage::Index,
    cat: &Catalog,
    qualify_in: Option<&Catalog>,
    i: usize,
    attrs_only: bool,
    pretty: bool,
) -> alloc::string::String {
    let col_at = |pos: usize| -> alloc::string::String {
        t.schema()
            .columns
            .get(pos)
            .map_or_else(|| "?".into(), |c| c.name.clone())
    };
    let opclasses = cat.index_opclasses(&idx.name);
    // v7.39.11 — PG omits the NULLS word when it matches the default for
    // the direction: ascending defaults to NULLS LAST and descending to
    // NULLS FIRST.
    let order_suffix = |descending: bool, nulls_first: Option<bool>| {
        let mut sfx = alloc::string::String::new();
        if descending {
            sfx.push_str(" DESC");
        }
        if let Some(nf) = nulls_first
            && nf != descending
        {
            sfx.push_str(if nf { " NULLS FIRST" } else { " NULLS LAST" });
        }
        sfx
    };
    let (collation, order) = if i == 0 {
        (
            idx.collation.as_deref(),
            spg_storage::KeyOrder {
                descending: idx.descending,
                nulls_first: idx.nulls_first,
            },
        )
    } else {
        (
            idx.extra_collations.get(i - 1).and_then(Option::as_deref),
            idx.extra_orders.get(i - 1).copied().unwrap_or_default(),
        )
    };
    let mut out = match crate::index_def::key_parts(idx).swap_remove(i) {
        crate::index_def::KeyPart::Column(p) => col_at(p),
        crate::index_def::KeyPart::Expression(expr) => {
            let text = crate::catalog_deparse::predicate_text_in(
                expr,
                &t.schema().columns,
                qualify_in,
                pretty,
            )
            .unwrap_or_else(|| alloc::string::String::from(expr));
            // v7.39 (read01 round 83) — PG double-parenthesises an
            // operator expression (`((a + b))`) but not a function
            // call (`lower(name)`); the stored form of the first
            // already opens with `(`.
            // 9.0.0 — the pretty form keeps ONE pair: the expression
            // comes back unparenthesised there, so this is where the
            // pair a key expression always carries is put on.
            if pretty {
                // Measured on 18.6: the pretty key carries exactly one
                // pair, and a call carries none — `(a || 'x'::text)`,
                // `(b::text)`, `(- b)`, `(a IS NULL)`, but `abs(b)` and
                // `COALESCE(a, 'z'::text)`. Decided from the node, not
                // from the text: `lower('Q'::text || a)` is a call whose
                // rendering looks like an operator expression.
                let is_call = matches!(
                    spg_sql::parser::parse_expression(expr),
                    Ok(spg_sql::ast::Expr::FunctionCall { .. })
                );
                if is_call {
                    text
                } else {
                    alloc::format!("({text})")
                }
            } else if text.starts_with('(') {
                alloc::format!("({text})")
            } else {
                text
            }
        }
    };
    // `pg_get_indexdef(oid, n)` prints the key alone (measured: column 2
    // of `(… , p DESC)` is `p`).
    if attrs_only {
        return out;
    }
    if let Some(c) = collation {
        out.push_str(&alloc::format!(" COLLATE \"{c}\""));
    }
    if let Some(Some(op)) = opclasses.get(i)
        && !crate::opclass::is_default(op)
    {
        out.push(' ');
        out.push_str(op);
    }
    out.push_str(&order_suffix(order.descending, order.nulls_first));
    out
}

pub(crate) fn render_indexdef(
    t: &spg_storage::Table,
    idx: &spg_storage::Index,
    tname: &str,
    cat: &Catalog,
    qualify: bool,
) -> alloc::string::String {
    render_indexdef_in(t, idx, tname, cat, qualify, false)
}

/// [`render_indexdef`], in the pretty form when `pretty`. See
/// [`catalog_indexdef_in`].
pub(crate) fn render_indexdef_in(
    t: &spg_storage::Table,
    idx: &spg_storage::Index,
    tname: &str,
    cat: &Catalog,
    qualify: bool,
    pretty: bool,
) -> alloc::string::String {
    let qualify_in = qualify.then_some(cat);
    let col_at = |pos: usize| -> alloc::string::String {
        t.schema()
            .columns
            .get(pos)
            .map_or_else(|| "?".into(), |c| c.name.clone())
    };
    let mut positions = alloc::vec![idx.column_position];
    positions.extend(idx.extra_column_positions.iter().copied());
    // v7.39 (round 537) — the leading key's ordering clause. PG prints
    // only what is NOT the default, and the nulls default flips with the
    // direction: LAST for ascending, FIRST for descending (measured
    // across all eight spellings). It decorates whichever form the key
    // took — a bare column is stored as an expression here, so applying
    // it to the column list alone would have been dropped again.
    // v7.39 (round 538) — an explicit COLLATE on the key is printed.
    //
    // Measured: PG shows `(a COLLATE "C")` even on a C-collation
    // database, because a named collation and the one a column inherits
    // are different OBJECTS even where they sort identically. It stops
    // showing it only once the COLUMN itself is declared with that same
    // collation — and `ColumnSchema` keeps a collation ENUM, not the
    // name it was declared under, so SPG cannot tell that case apart.
    // Recorded rather than guessed: the rarer spelling over-prints.
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
    // 8.0.3 — the key expression in catalog form (a varchar argument's
    // implicit cast, a typed literal), with a user function qualified when
    // asked; see `catalog_deparse`.
    // 9.0.0 — one renderer for every key part, column or expression, in
    // any position. The leading part used to be the only one that could
    // carry an expression, a collation or an operator class, so
    // `(p, lower(email) text_pattern_ops)` could not even be represented.
    let key = (0..positions.len())
        .map(|i| render_index_part_in(t, idx, cat, qualify_in, i, false, pretty))
        .collect::<Vec<_>>()
        .join(", ");
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
        // v7.38.1 (L12) — a multi-column B-tree IS a btree to every
        // catalog surface; the composite key is an implementation
        // detail the dump must not see.
        spg_storage::IndexKind::BTree(_) | spg_storage::IndexKind::BTreeMulti(_) => "btree",
    };
    // 9.0.0 — and the method the statement ASKED for wins over the one
    // underneath, when SPG recorded one. `USING gist` came back as
    // `USING btree`, so a dump restored into PostgreSQL as a different
    // index type. See `Index::declared_am`.
    let am: &str = idx.declared_am.as_deref().unwrap_or(am);
    // 9.0.0 — the pretty form drops the schema qualification, as PG's
    // does; the plain one always writes it.
    let schema = if pretty { "" } else { "public." };
    match &idx.partial_predicate {
        Some(pred) => {
            // 8.0.3 — PG's catalog form (see `catalog_deparse`): a bare
            // boolean column is not parenthesised, a literal is typed.
            let wrapped = crate::catalog_deparse::predicate_text_in(
                pred,
                &t.schema().columns,
                qualify_in,
                pretty,
            )
            .unwrap_or_else(|| {
                let p = pred.trim();
                if pretty || (p.starts_with('(') && p.ends_with(')')) {
                    alloc::string::String::from(p)
                } else {
                    alloc::format!("({p})")
                }
            });
            alloc::format!(
                "CREATE {unique_kw}INDEX {} ON {schema}{tname} USING {am} ({key}){nnd} WHERE {wrapped}",
                idx.name,
            )
        }
        None => alloc::format!(
            "CREATE {unique_kw}INDEX {} ON {schema}{tname} USING {am} ({key}){nnd}",
            idx.name,
        ),
    }
}

pub(crate) fn synth_pg_indexes(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("schemaname", DataType::Name, false),
        ColumnSchema::new("tablename", DataType::Name, false),
        ColumnSchema::new("indexname", DataType::Name, false),
        // v7.39 (round 542) — PG's fourth column: NULL means the
        // database default tablespace, which is the only one SPG has.
        ColumnSchema::new("tablespace", DataType::Name, true),
        ColumnSchema::new("indexdef", DataType::Text, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    // v7.39.13 — through the one enumeration, so this view and
    // `pg_index` list the same objects. Listing only the storage
    // indexes here while `pg_index` also carries each constraint's own
    // row would be two catalogs disagreeing about one schema.
    for ci in catalog_indexes(cat) {
        let Some(t) = cat.get(&ci.table) else {
            continue;
        };
        {
            let tname = ci.table.clone();
            let indexdef = catalog_indexdef(cat, t, &ci, None);
            rows.push(Row::new(alloc::vec![
                Value::text("public"),
                Value::text(tname.clone()),
                Value::text(ci.name.clone()),
                Value::Null, // tablespace — the default one
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
        ColumnSchema::new("indexrelid", DataType::Oid, false),
        ColumnSchema::new("indrelid", DataType::Oid, false),
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
        // v7.39.11 — PG's own types for these four; see the row
        // builder below.
        ColumnSchema::new("indkey", DataType::Int2Vector, false),
        ColumnSchema::new("indcollation", DataType::OidVector, false),
        ColumnSchema::new("indclass", DataType::OidVector, false),
        ColumnSchema::new("indoption", DataType::Int2Vector, false),
        // v7.39 (round 543) — PG's last two, and the two a tool tests to
        // tell an expression index from a plain one and a partial index
        // from a whole one. Measured: both NULL for a plain index,
        // both non-NULL for `ON t ((a+1)) WHERE b <> ''`. SPG keeps the
        // written text rather than PG's node tree, which is the same
        // choice pg_attrdef.adbin already makes here.
        ColumnSchema::new("indexprs", DataType::PgNodeTree, true),
        ColumnSchema::new("indpred", DataType::PgNodeTree, true),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    let mut idx_oid: i64 = 100_000;
    // Build a name → user-relation OID map so indrelid matches
    // pg_class.oid (synth_pg_class starts at 16384). Without
    // this, joins between pg_index and pg_class fail.
    let names = cat.visible_relation_keys();
    let mut table_oid: i64 = 16384;
    let mut by_table: alloc::collections::BTreeMap<String, i64> =
        alloc::collections::BTreeMap::new();
    for tname in &names {
        by_table.insert(tname.clone(), table_oid);
        table_oid = table_oid.saturating_add(1);
    }
    // v7.39.13 — one enumeration, and nothing guesses which constraint
    // an index backs. See `catalog_indexes`: a storage index reports its
    // own columns and its own uniqueness, and each uniqueness constraint
    // gets the row PostgreSQL gives it. The prefix match this replaces
    // reported two primary keys for one table and told a reader that an
    // index accepting duplicates was unique.
    for ci in catalog_indexes(cat) {
        {
            let relid = *by_table.get(&ci.table).unwrap_or(&0);
            let idx_oid = ci.oid;
            // PG's `indkey` int2vector — column positions, 1-based.
            // SPG stores positions 0-based; add 1 to align with PG's
            // attnum.
            //
            // v7.39.11 — a real `int2vector`, not the text that prints
            // the same. Reported by sentori against 7.39.10: through
            // that version these four were `text`, so `a.attnum = ANY
            // (i.indkey)` — how Django's introspection, Rails' schema
            // dumper, sqlalchemy and every hand-written schema-diff
            // query ask which columns an index covers — raised
            // "ANY/ALL right-hand side must be an array, got text".
            // 9.0.0 — an expression part is attribute 0 (measured on PG
            // 18.6: `(p, lower(a || b))` is `1 0`), then the INCLUDE
            // columns, which `indnatts` counts and `indnkeyatts` does not.
            let n_key = ci.columns.len();
            let n_attrs_total = n_key + ci.included.len();
            let indkey: Vec<i16> = ci
                .columns
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    if ci.part_expressions.get(i).is_some_and(Option::is_some) {
                        0
                    } else {
                        i16::try_from(p + 1).unwrap_or(0)
                    }
                })
                .chain(
                    ci.included
                        .iter()
                        .map(|p| i16::try_from(p + 1).unwrap_or(0)),
                )
                .collect();
            // 9.0.0 — 100 (`default`) for a part whose type collates, 0
            // otherwise; one entry per KEY part.
            let tschema = cat.get(&ci.table).map(|t| t.schema());
            let collatable = |ty: spg_storage::DataType| {
                matches!(
                    ty,
                    spg_storage::DataType::Text
                        | spg_storage::DataType::Varchar(_)
                        | spg_storage::DataType::Char(_)
                        | spg_storage::DataType::Name
                )
            };
            // The type each key part indexes: the column's, or what the
            // expression evaluates to. `indcollation` and `indclass` both
            // answer from it.
            let part_types: Vec<Option<spg_storage::DataType>> = (0..n_key)
                .map(|i| {
                    let schema = tschema?;
                    match ci.part_expressions.get(i).and_then(Option::as_deref) {
                        Some(src) => spg_sql::parser::parse_expression(src)
                            .ok()
                            .and_then(|e| {
                                crate::describe::describe_expr_in(&e, &schema.columns, Some(cat))
                            })
                            .map(|shape| shape.ty),
                        None => schema.columns.get(ci.columns[i]).map(|c| c.ty),
                    }
                })
                .collect();
            // 9.0.0 — the operator class oid per KEY part, which is what a
            // client joins to `pg_opclass` to learn how an index compares.
            // These were all zero, so the join found nothing: `\d`'s
            // opclass column, and every schema-diff tool that reads one,
            // saw an index with no comparison semantics at all.
            let am_name = am_name_of_oid(ci.am_oid);
            let indclass: Vec<u32> = (0..n_key)
                .map(|i| {
                    let named = ci
                        .part_opclasses
                        .get(i)
                        .and_then(Option::as_deref)
                        .and_then(|name| pg_opclass_oid(am_name, name));
                    let resolved = named.or_else(|| {
                        let ty = part_types.get(i).copied().flatten()?;
                        // A polymorphic default: PostgreSQL files one row
                        // for `anyarray`, not one per element type, and
                        // answers with it for an array column.
                        let key = if crate::conversions::array_element_type(ty).is_some() {
                            2277
                        } else {
                            pg_type_oid(ty)
                        };
                        pg_default_opclass_oid(am_name, key)
                    });
                    u32::try_from(resolved.unwrap_or(0)).unwrap_or(0)
                })
                .collect();
            let indcollation: Vec<u32> = (0..n_key)
                .map(|i| {
                    if part_types.get(i).copied().flatten().is_some_and(collatable) {
                        100
                    } else {
                        0
                    }
                })
                .collect();
            // 9.0.0 — PG's per-part option bits: DESC is 1, NULLS FIRST 2
            // (so a bare `DESC`, whose nulls default to first, is 3).
            let indoption: Vec<i16> = (0..n_key)
                .map(|i| {
                    let o = ci.part_orders.get(i).copied().unwrap_or_default();
                    let nulls_first = o.nulls_first.unwrap_or(o.descending);
                    i16::from(o.descending) | (i16::from(nulls_first) << 1)
                })
                .collect();
            let n_attrs_total_key = n_key;
            let is_primary = ci.is_primary;
            let is_unique = ci.is_unique;
            let is_partial = ci.partial_predicate.is_some();
            let is_expression = ci.expression.is_some();
            let _ = (is_partial, is_expression);
            rows.push(Row::new(alloc::vec![
                Value::BigInt(idx_oid),
                Value::BigInt(relid),
                Value::SmallInt(i16::try_from(n_attrs_total).unwrap_or(i16::MAX)),
                Value::SmallInt(i16::try_from(n_attrs_total_key).unwrap_or(i16::MAX)),
                Value::Bool(is_unique),
                // v7.39 (round 473) — the index has carried this since
                // round 52 and enforces it; only the catalog was still
                // answering `f`, so a migration tool reading pg_index saw
                // a plain unique index and would have tried to "fix" it.
                Value::Bool(ci.nulls_not_distinct),
                Value::Bool(is_primary),
                Value::Bool(false), // indisexclusion — EXCLUDE constraint
                Value::Bool(true),  // indimmediate
                Value::Bool(false), // indisclustered
                Value::Bool(true),  // indisvalid
                Value::Bool(false), // indcheckxmin
                Value::Bool(true),  // indisready
                Value::Bool(true),  // indislive
                Value::Bool(false), // indisreplident
                Value::Int2Vector(indkey),
                Value::OidVector(indcollation),
                Value::OidVector(indclass),
                Value::Int2Vector(indoption),
                {
                    // indexprs — every expression part, in key order, as
                    // `pg_get_expr` prints a list.
                    let exprs: Vec<&str> = ci
                        .part_expressions
                        .iter()
                        .filter_map(Option::as_deref)
                        .collect();
                    if exprs.is_empty() {
                        Value::Null
                    } else {
                        Value::text(exprs.join(", "))
                    }
                },
                ci.partial_predicate
                    .clone()
                    .map_or(Value::Null, Value::text), // indpred
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
    // 8.0.3 — `public` belongs to `pg_database_owner` (6171), as in every
    // PostgreSQL since 15; the other schemas to the bootstrap superuser.
    // It read 10, so `pg_dump` wrote `ALTER SCHEMA public OWNER TO
    // postgres` and a GRANT/REVOKE block for an owner PostgreSQL does not
    // have.
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::Oid, false),
        ColumnSchema::new("nspname", DataType::Name, false),
        ColumnSchema::new("nspowner", DataType::Oid, false),
        // v7.39 (read01 round 60) — the schema ACL. Never NULL for `public`:
        // PG ships it with PUBLIC holding USAGE (but NOT create).
        ColumnSchema::new("nspacl", DataType::AclItemArray, true),
    ];
    let public_acl = crate::acl::render_nspacl(cat);
    // v7.38.14 — every session that owns a temporary relation also owns a
    // `pg_temp_N` namespace, and `pg_class.relnamespace` now points at it.
    // Without the matching row the join simply drops those relations, which
    // would trade one wrong answer for a missing one. Derived from the
    // catalog's own names rather than tracked separately, so a namespace
    // cannot outlive the objects that put it here.
    let mut temp_ns: alloc::vec::Vec<u32> = cat
        .table_names()
        .iter()
        .filter_map(|n| crate::Engine::temp_session_of(n.as_str()))
        .chain(
            cat.sequences_all()
                .keys()
                .filter_map(|n| crate::Engine::temp_session_of(n)),
        )
        .chain(
            cat.views_all()
                .keys()
                .filter_map(|n| crate::Engine::temp_session_of(n)),
        )
        .collect();
    temp_ns.sort_unstable();
    temp_ns.dedup();
    let mut rows = alloc::vec![
        Row::new(alloc::vec![
            Value::BigInt(11),
            Value::text("pg_catalog"),
            Value::BigInt(10),
            Value::Null,
        ]),
        Row::new(alloc::vec![
            Value::BigInt(2200),
            Value::text("public"),
            Value::BigInt(crate::role_directory::DATABASE_OWNER_OID),
            Value::text(public_acl),
        ]),
        Row::new(alloc::vec![
            Value::BigInt(13000),
            Value::text("information_schema"),
            Value::BigInt(10),
            Value::Null,
        ]),
        // v7.39 (round 661) — F34. SPG answers 86 functions PG18 does not
        // have: the MySQL-dialect family, four `spg_*` of its own, the
        // extension families (uuid-ossp, pg_trgm, pg_prewarm) and fifty
        // `pg_*` names PG never had — `pg_stat_get_idx_scan` against PG's
        // `pg_stat_get_numscans`, `pg_start_backup` which PG15 removed.
        // Every one of them is callable (measured, 86/86), so removing the
        // rows would cost discoverability; leaving them at
        // `pronamespace = 11` claims PostgreSQL provides them, which is the
        // same lie round 653 refused when it kept 149 dialect names OUT of
        // pg_proc. PG's own answer for functions core does not have is a
        // different namespace — that is where extension functions live —
        // so that is what these get.
        Row::new(alloc::vec![
            Value::BigInt(13500),
            Value::text("pg_spg"),
            Value::BigInt(10),
            Value::Null,
        ]),
    ];
    // 9.0.0 (C9) — every schema `CREATE SCHEMA` made. The registry has
    // held these names since v7.17.0 and nothing published them, so a
    // client asking whether its schema exists was told no while
    // `CREATE SCHEMA … IF NOT EXISTS` said it already did.
    for name in cat.user_schemas() {
        let Some(oid) = user_schema_oid(cat, name) else {
            continue;
        };
        rows.push(Row::new(alloc::vec![
            Value::BigInt(oid),
            Value::text(name.clone()),
            Value::BigInt(10),
            Value::Null,
        ]));
    }
    for sid in temp_ns {
        rows.push(Row::new(alloc::vec![
            Value::BigInt(TEMP_NS_OID_BASE + i64::from(sid)),
            Value::text(alloc::format!("pg_temp_{sid}")),
            Value::BigInt(10),
            Value::Null,
        ]));
    }
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
    (
        "pg_indexes",
        &["indexname", "schemaname", "tablename", "tablespace"],
    ),
    (
        "pg_matviews",
        &["matviewname", "matviewowner", "schemaname", "tablespace"],
    ),
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
    (
        "pg_stat_user_indexes",
        &["indexrelname", "relname", "schemaname"],
    ),
    ("pg_stat_user_tables", &["relname", "schemaname"]),
    ("pg_statistic_ext", &["stxname"]),
    ("pg_subscription", &["subname", "subslotname"]),
    (
        "pg_tables",
        &["schemaname", "tablename", "tableowner", "tablespace"],
    ),
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
        if let Some((_, base)) = INFORMATION_SCHEMA_DOMAINS.iter().find(|(n, _)| n == domain) {
            c.ty = *base;
        }
    }
}

/// 9.0.0 (N19) — the ONE list, from `spg_sql::catalog_registry`.
///
/// This was a 43-name copy beside the parser's 107-name gate, and the
/// difference was observable: 64 names answered a query and had no
/// `pg_class` row. `SELECT count(*) FROM pg_database` answered 1 while
/// `SELECT count(*) FROM pg_class WHERE relname = 'pg_database'`
/// answered 0; PostgreSQL 18.6 answers 7 and 1.
pub(crate) use spg_sql::catalog_registry::CATALOG_RELATIONS;

/// The columns one of those relations has.
///
/// Taken from the synth itself rather than written out again here: a second
/// copy of twenty-two column lists would drift from the relations it claims
/// to describe the first time one of them gains a column, and drift is
/// exactly the failure this is meant to fix. The rows the synths build on
/// the way are discarded — `pg_attribute` is introspection, not a hot path,
/// and every one of these is either fixed-size or O(tables), which is what
/// `pg_attribute` itself already costs.
/// v7.39 (round 663) — is this one of the catalogs SPG synthesises?
///
/// `has_column_privilege('pg_class'::regclass, 'oid', 'SELECT')` answered
/// `relation "pg_class" does not exist`, because the privilege check looks
/// the name up in the user catalog and a synthesised view is not there. PG
/// answers `t` — the system catalogs are readable by PUBLIC. The predicate
/// is the same dispatch the column reader uses, so a catalog cannot be
/// visible to one and invisible to the other.
pub(crate) fn is_synthesised_catalog(name: &str, cat: &Catalog) -> bool {
    catalog_relation_columns(name, cat).is_some()
}

fn catalog_relation_columns(name: &str, cat: &Catalog) -> Option<Vec<ColumnSchema>> {
    Some(match name {
        "pg_am" => synth_pg_am(cat).0,
        "pg_amop" => synth_pg_amop(cat).0,
        "pg_amproc" => synth_pg_amproc(cat).0,
        "pg_attrdef" => synth_pg_attrdef(cat).0,
        "pg_attribute" => pg_attribute_schema(),
        "pg_cast" => synth_pg_cast().0,
        "pg_class" => {
            let mut c = pg_class_schema();
            splice_pg_class_v18_schema(&mut c);
            c
        }
        "pg_collation" => synth_pg_collation(cat).0,
        "pg_constraint" => synth_pg_constraint(cat).0,
        "pg_depend" => synth_pg_depend(cat).0,
        "pg_enum" => synth_pg_enum(cat).0,
        "pg_extension" => synth_pg_extension(cat).0,
        "pg_index" => synth_pg_index_raw(cat).0,
        "pg_inherits" => synth_pg_inherits(cat).0,
        "pg_ts_config" => synth_pg_ts_config(cat).0,
        "pg_ts_config_map" => synth_pg_ts_config_map(cat).0,
        "pg_ts_dict" => synth_pg_ts_dict(cat).0,
        "pg_ts_parser" => synth_pg_ts_parser(cat).0,
        "pg_ts_template" => synth_pg_ts_template(cat).0,
        "pg_largeobject" => synth_pg_largeobject(cat).0,
        "pg_largeobject_metadata" => synth_pg_largeobject_metadata(cat).0,
        "pg_namespace" => synth_pg_namespace(cat).0,
        "pg_opclass" => synth_pg_opclass(cat).0,
        "pg_opfamily" => synth_pg_opfamily(cat).0,
        "pg_operator" => synth_pg_operator(cat).0,
        "pg_policy" => {
            synth_pg_policy(cat, &crate::role_directory::RoleDirectory::for_shape_only()).0
        }
        "pg_proc" => synth_pg_proc(cat, &crate::role_directory::RoleDirectory::for_shape_only()).0,
        "pg_statistic" => synth_pg_statistic(cat, &crate::statistics::Statistics::new()).0,
        "pg_stats" => synth_pg_stats(cat, &crate::statistics::Statistics::new()).0,
        "pg_statistic_ext" => synth_pg_statistic_ext(cat).0,
        "pg_tablespace" => synth_pg_tablespace(cat).0,
        "pg_trigger" => synth_pg_trigger(cat).0,
        "pg_type" => synth_pg_type(cat, &crate::role_directory::RoleDirectory::for_shape_only()).0,
        // 9.0.0 — the catalog VIEWS. They answered queries and appeared
        // in no catalog, so `pg_class` had no row and
        // `information_schema` could not list them. The four that
        // otherwise need an `Engine` publish their shape on its own, so
        // listing a relation's columns never builds its rows.
        "pg_views" => {
            synth_pg_views(cat, &crate::role_directory::RoleDirectory::for_shape_only()).0
        }
        "pg_tables" => {
            synth_pg_tables(cat, &crate::role_directory::RoleDirectory::for_shape_only()).0
        }
        "pg_matviews" => {
            synth_pg_matviews(cat, &crate::role_directory::RoleDirectory::for_shape_only()).0
        }
        "pg_sequences" => {
            synth_pg_sequences(cat, &crate::role_directory::RoleDirectory::for_shape_only()).0
        }
        "pg_indexes" => synth_pg_indexes(cat).0,
        "pg_rules" => synth_pg_rules(cat).0,
        "pg_roles" => pg_roles_schema(),
        "pg_user" => pg_user_schema(),
        "pg_settings" => pg_settings_schema(),
        "pg_prepared_statements" => pg_prepared_statements_schema(),
        "pg_auth_members" => pg_auth_members_schema(),
        "pg_authid" => pg_authid_schema(),
        "pg_database" => pg_database_schema(),
        "pg_db_role_setting" => pg_db_role_setting_schema(),
        "pg_description" => pg_description_schema(),
        "pg_foreign_table" => pg_foreign_table_schema(),
        "pg_group" => pg_group_schema(),
        "pg_language" => pg_language_schema(),
        "pg_locks" => crate::spg_admin::pg_locks_schema(),
        "pg_partitioned_table" => pg_partitioned_table_schema(),
        "pg_policies" => pg_policies_schema(),
        "pg_publication" => pg_publication_schema(),
        "pg_range" => pg_range_schema(),
        "pg_replication_slots" => pg_replication_slots_schema(),
        "pg_rewrite" => pg_rewrite_schema(),
        "pg_sequence" => pg_sequence_schema(),
        // `pg_shadow` is `pg_user`'s shape, as PostgreSQL defines it.
        "pg_shadow" => pg_user_schema(),
        "pg_stat_activity" => crate::spg_admin::pg_stat_activity_schema(),
        "pg_stat_archiver" => pg_stat_archiver_schema(),
        "pg_stat_bgwriter" => pg_stat_bgwriter_schema(),
        "pg_stat_checkpointer" => pg_stat_checkpointer_schema(),
        "pg_stat_database" => pg_stat_database_schema(),
        "pg_stat_io" => pg_stat_io_schema(),
        "pg_stat_progress_analyze" => pg_stat_progress_analyze_schema(),
        "pg_stat_progress_create_index" => pg_stat_progress_create_index_schema(),
        "pg_stat_progress_vacuum" => pg_stat_progress_vacuum_schema(),
        "pg_stat_replication" => pg_stat_replication_schema(),
        "pg_stat_slru" => pg_stat_slru_schema(),
        "pg_stat_subscription_stats" => pg_stat_subscription_stats_schema(),
        "pg_stat_user_functions" => pg_stat_user_functions_schema(),
        "pg_stat_user_indexes" => pg_stat_user_indexes_schema(),
        "pg_stat_user_tables" => pg_stat_user_tables_schema(),
        "pg_stat_wal" => pg_stat_wal_schema(),
        "pg_statio_user_tables" => crate::spg_admin::pg_statio_user_tables_schema(),
        "pg_subscription" => pg_subscription_schema(),
        "pg_timezone_abbrevs" => pg_timezone_abbrevs_schema(),
        "pg_timezone_names" => pg_timezone_names_schema(),
        // 9.0.0 (N19) — the catalogs SPG is genuinely empty of carry
        // their columns in `EMPTY_PG_CATALOGS`, and those columns were
        // reaching the query path and not `pg_attribute`: the relation
        // was published and not self-described.
        other => match synth_empty_pg_catalog(&alloc::format!("__spg_{other}")) {
            Some((cols, _)) => cols,
            None => return None,
        },
    })
}

fn relation_oid_for_meta_view(name: &str) -> i64 {
    let bare = name
        .strip_prefix("__spg_pg_")
        .map(|b| alloc::format!("pg_{b}"))
        .or_else(|| {
            name.strip_prefix("__spg_info_")
                .map(alloc::string::String::from)
        })
        .unwrap_or_else(|| alloc::string::String::from(name));
    // The oids PG assigns its catalogs; anything else — an
    // information_schema view, a pg_stat_* view — has no fixed oid and
    // reports 0, which is what a relation with no entry reports.
    //
    // 8.0.3 — every one of them. The list named eight, so `tableoid` read 0
    // on `pg_attrdef`, `pg_depend`, `pg_extension` and the rest, and
    // `pg_dump`, which identifies each catalog object by
    // `(tableoid, oid)`, could not match a dependency to the object it was
    // about: a serial column's default went inline, ahead of its sequence.
    // 9.0.0 (N19) — the ONE list. This was a 64-name copy of it, and a
    // catalog present in one and absent from the other reported
    // `tableoid` 0.
    spg_sql::catalog_registry::catalog_relation_oid(&bare).unwrap_or(0)
}

pub(crate) fn materialise_meta_view(
    catalog: &mut Catalog,
    name: &str,
    mut columns: Vec<ColumnSchema>,
    rows: Vec<Row<'static>>,
) -> Result<(), EngineError> {
    // v7.39 (round 543) — a synth that widens its SCHEMA and forgets a
    // row builder compiles cleanly, because a row is an untyped value
    // list. Round 542 shipped that mistake twice before a probe caught
    // it; this catches it at the one place every catalog passes through.
    debug_assert!(
        rows.iter().all(|r| r.values.len() == columns.len()),
        "{name}: {} columns but a row has {}",
        columns.len(),
        rows.iter()
            .map(|r| r.values.len())
            .find(|n| *n != columns.len())
            .unwrap_or(0)
    );
    retype_identifier_columns(name, &mut columns);
    apply_information_schema_domains(name, &mut columns);
    // v7.39 (round 540) — every catalog relation carries the system
    // columns, as PG's do.
    //
    // Round 512 materialised them for a user table on the bare-select
    // scan, which is not where a catalog is read from: a synthesized
    // view becomes a real relation here, and the join path builds its
    // schema from that relation's columns. So `SELECT x.tableoid FROM
    // pg_extension x` answered and the same reference through a JOIN
    // said the column does not exist — which is where `pg_dump` stopped,
    // its extension query being exactly that shape.
    //
    // Appending them once, here, is what makes them resolve wherever the
    // relation is read. `*` skips the trailing six by POSITION (round
    // 512's rule, so a genuine `xmin` column is not lost), and that skip
    // now sees them through a join's `alias.column` naming too.
    let sys_start = columns.len();
    for sys in crate::select::SYSTEM_COLUMNS {
        columns.push(ColumnSchema::new(
            alloc::string::String::from(sys),
            DataType::Text,
            false,
        ));
    }
    let view_oid = relation_oid_for_meta_view(name);
    let schema = TableSchema::new(name.to_string(), columns);
    catalog.create_table(schema).map_err(EngineError::Storage)?;
    let table = catalog
        .get_mut(name)
        .expect("just-created meta view must exist");
    for (i, mut row) in rows.into_iter().enumerate() {
        row.values.truncate(sys_start);
        // One block, offsets from 1, as PG numbers them; a catalog row
        // is frozen, which is what PG reports for one too.
        row.values
            .push(Value::text(alloc::format!("(0,{})", i + 1)));
        row.values.push(Value::text("2")); // xmin — FrozenTransactionId
        row.values.push(Value::text("0")); // cmin
        row.values.push(Value::text("0")); // xmax
        row.values.push(Value::text("0")); // cmax
        row.values.push(Value::text(alloc::format!("{view_oid}")));
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

/// 7.38.1 S5.2 — a partition bound in PG's SQL literal form (what
/// `pg_get_expr(relpartbound, oid)` prints and pg_dump replays).
///
/// 8.0.3 — every temporal bound is held as an instant, so the key column's
/// type decides the spelling: PG 18.6 prints `'2026-01-01'` for a `date`
/// key and `'2026-06-01 12:30:00'` for a `timestamp` one. Both used to come
/// out as timestamptz text, `'2026-01-01 00:00:00+00'`.
pub(crate) fn partition_bound_sql(
    b: &spg_storage::PartitionBound,
    key: Option<DataType>,
) -> String {
    use spg_storage::PartitionBound as B;
    match b {
        B::MinValue => String::from("MINVALUE"),
        B::MaxValue => String::from("MAXVALUE"),
        B::TimestampTz(us) => match key {
            Some(DataType::Date) => alloc::format!(
                "'{}'",
                crate::eval::format_date(
                    i32::try_from(us.div_euclid(86_400_000_000)).unwrap_or(i32::MAX)
                )
            ),
            Some(DataType::Timestamp) => {
                alloc::format!("'{}'", crate::eval::format_timestamp(*us))
            }
            _ => alloc::format!("'{}'", crate::eval::format_timestamptz(*us)),
        },
        B::BigInt(n) => alloc::format!("'{n}'"),
        B::Int(n) => alloc::format!("'{n}'"),
        B::SmallInt(n) => alloc::format!("'{n}'"),
        B::Date(d) => alloc::format!("'{}'", crate::eval::format_date(*d)),
        B::Text(s) => alloc::format!("'{}'", s.replace('\'', "''")),
    }
}

/// 7.38.1 S5.2 — `pg_class.relpartbound` deparse for a partition
/// child, PG's exact clause shape. `None` for parents / plain tables.
pub(crate) fn relpartbound_text(
    cat: &Catalog,
    role: &spg_storage::PartitionRole,
) -> Option<String> {
    use spg_storage::PartitionRole as R;
    // The parent's first key column's type.
    let key_type = |parent: &str| -> Option<DataType> {
        let t = cat.get(parent)?;
        let R::Parent {
            key_column_positions,
            ..
        } = t.schema().partition_role.as_ref()?
        else {
            return None;
        };
        let pos = *key_column_positions.first()?;
        t.schema().columns.get(pos).map(|c| c.ty)
    };
    Some(match role {
        R::Parent { .. } => return None,
        R::Range {
            parent_name,
            lower,
            upper,
            ..
        } => {
            let key = key_type(parent_name);
            alloc::format!(
                "FOR VALUES FROM ({}) TO ({})",
                partition_bound_sql(lower, key),
                partition_bound_sql(upper, key)
            )
        }
        R::List {
            parent_name,
            values,
            ..
        } => {
            let key = key_type(parent_name);
            let items: Vec<String> = values.iter().map(|v| partition_bound_sql(v, key)).collect();
            alloc::format!("FOR VALUES IN ({})", items.join(", "))
        }
        R::Hash {
            modulus, remainder, ..
        } => alloc::format!("FOR VALUES WITH (modulus {modulus}, remainder {remainder})"),
        R::Default { .. } => String::from("DEFAULT"),
        // Inheritance children are not partitions; no bound clause.
        R::Inherits { .. } => return None,
    })
}

/// 7.38.1 S5.2 — `pg_get_partkeydef(oid)`: "RANGE (ts)" and friends,
/// from the parent's own partition metadata. `None` when the oid does
/// not name a partitioned parent.
pub(crate) fn partkey_def_text(cat: &Catalog, oid: i64) -> Option<String> {
    let name = relation_name_for_oid(cat, oid)?;
    let t = cat.get(&name)?;
    let spg_storage::PartitionRole::Parent {
        kind,
        key_column_positions,
        ..
    } = t.schema().partition_role.as_ref()?
    else {
        return None;
    };
    let strat = match kind {
        spg_storage::PartitionKind::Range => "RANGE",
        spg_storage::PartitionKind::List => "LIST",
        spg_storage::PartitionKind::Hash => "HASH",
    };
    let cols: Vec<String> = key_column_positions
        .iter()
        .filter_map(|p| t.schema().columns.get(*p).map(|c| c.name.clone()))
        .collect();
    Some(alloc::format!("{strat} ({})", cols.join(", ")))
}
