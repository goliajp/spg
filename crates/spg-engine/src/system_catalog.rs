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
pub(crate) fn pg_check_connames(t: &spg_storage::Table, tname: &str, checks: &[String]) -> Vec<String> {
    let mut seen: alloc::collections::BTreeMap<String, usize> =
        alloc::collections::BTreeMap::new();
    let mut out = Vec::with_capacity(checks.len());
    for chk in checks {
        let cols = referenced_columns(t, chk);
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
        DataType::Bit => "bit",
        DataType::BitVarying => "bit varying",
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
        ColumnSchema::new("numeric_scale", DataType::Int, true),
        ColumnSchema::new("udt_name", DataType::Text, false),
        ColumnSchema::new("is_identity", DataType::Text, false),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    for tname in cat.table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        for (i, col) in t.schema().columns.iter().enumerate() {
            #[allow(clippy::cast_possible_wrap)]
            let ordinal = (i + 1) as i32;
            // column_default: runtime expressions keep their SQL
            // text; literal defaults render via Display.
            let default_text: Value<'static> = if let Some(expr) =
                &col.runtime_default
            {
                Value::text(expr.clone())
            } else if let Some(v) = &col.default {
                // Render the literal default's SQL form.
                let rendered = match v {
                    Value::Text(s) => alloc::format!("'{s}'::text"),
                    Value::Int(n) => n.to_string(),
                    Value::BigInt(n) => n.to_string(),
                    Value::SmallInt(n) => n.to_string(),
                    Value::Float(f) => f.to_string(),
                    Value::Bool(b) => b.to_string(),
                    // Typed literal defaults render via the engine's
                    // canonical text formatters (PG stores the default
                    // expression text) instead of leaking Rust Debug
                    // (`Numeric { scaled: 0, scale: 2 }`).
                    Value::Numeric { scaled, scale } => {
                        crate::eval::format_numeric(*scaled, *scale)
                    }
                    Value::Date(d) => alloc::format!("'{}'::date", crate::eval::format_date(*d)),
                    Value::Timestamp(t) => {
                        alloc::format!("'{}'::timestamp", crate::eval::format_timestamp(*t))
                    }
                    Value::Uuid(b) => alloc::format!("'{}'::uuid", spg_storage::format_uuid(b)),
                    other => alloc::format!("{other:?}"),
                };
                Value::text(rendered)
            } else if col.auto_increment {
                Value::text(alloc::format!(
                    "nextval('{tname}_{}_seq'::regclass)",
                    col.name
                ))
            } else {
                Value::Null
            };
            let (num_prec, num_scale): (Value<'static>, Value<'static>) =
                match col.ty {
                    DataType::SmallInt => (Value::Int(16), Value::Int(0)),
                    DataType::Int => (Value::Int(32), Value::Int(0)),
                    DataType::BigInt => (Value::Int(64), Value::Int(0)),
                    DataType::Float => (Value::Int(53), Value::Null),
                    DataType::Numeric { precision, scale } => (
                        Value::Int(i32::from(precision)),
                        Value::Int(i32::from(scale)),
                    ),
                    _ => (Value::Null, Value::Null),
                };
            // udt_name is PG's internal typname (int4, not integer).
            let udt: &str = match col.ty {
                DataType::SmallInt => "int2",
                DataType::Int => "int4",
                DataType::BigInt => "int8",
                DataType::Float => "float8",
                DataType::Bool => "bool",
                DataType::Text => "text",
                DataType::Bytes => "bytea",
                DataType::Json => "jsonb",
                DataType::Uuid => "uuid",
                DataType::Date => "date",
                DataType::Timestamp => "timestamp",
                _ => "text",
            };
            rows.push(Row::new(alloc::vec![
                Value::text("spg"),
                Value::text("public"),
                Value::text(tname.clone()),
                Value::text(col.name.clone()),
                Value::Int(ordinal),
                Value::text::<&str>(if col.nullable { "YES" } else { "NO" }),
                Value::text(pg_data_type_text(col.ty)),
                default_text,
                Value::Null, // character_maximum_length (SPG TEXT is unbounded)
                num_prec,
                num_scale,
                Value::text::<&str>(udt),
                Value::text::<&str>(if col.auto_increment { "YES" } else { "NO" }),
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
        .views()
        .iter()
        .map(|(_, v)| {
            Row::new(alloc::vec![
                Value::text("spg"),
                Value::text("public"),
                Value::text(v.name.clone()),
                Value::text(v.body.clone()),
                Value::text("NONE"),
                Value::text("NO"),  // is_updatable — view-update lands in 19.13
                Value::text("NO"),  // is_insertable_into
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
    for tname in cat.table_names() {
        by_name.insert(tname.clone(), oid);
        oid = oid.saturating_add(1);
    }
    let mut rows: Vec<Row<'static>> = Vec::new();
    // Track per-parent seqno so each child gets a unique 1-based
    // index — matches PG's pg_inherits.inhseqno semantics.
    let mut per_parent_seq: alloc::collections::BTreeMap<i64, i32> =
        alloc::collections::BTreeMap::new();
    for cname in cat.table_names() {
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
pub(crate) fn synth_pg_statistic_ext(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
    let schema = alloc::vec![
        ColumnSchema::new("oid", DataType::BigInt, false),
        ColumnSchema::new("stxrelid", DataType::BigInt, false),
        ColumnSchema::new("stxname", DataType::Text, false),
        ColumnSchema::new("stxnamespace", DataType::BigInt, false),
        ColumnSchema::new("stxowner", DataType::BigInt, false),
        ColumnSchema::new("stxkind", DataType::Text, false),
        ColumnSchema::new("stxkeys", DataType::Text, false),
    ];
    let rows: Vec<Row<'static>> = Vec::new();
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
    for name in cat.table_names() {
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
pub(crate) fn synth_pg_stat_replication(
    _cat: &Catalog,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
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
    for tname in cat.table_names() {
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
pub(crate) fn synth_pg_stat_user_tables(cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
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
        ColumnSchema::new("last_analyze", DataType::Timestamptz, true),
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    let mut relid: i64 = 16384; // PG user-relation OID floor
    for name in cat.table_names() {
        if crate::is_internal_table_name(&name) {
            continue;
        }
        let Some(t) = cat.get(&name) else {
            continue;
        };
        let live_rows = t.rows().len() as i64;
        rows.push(Row::new(alloc::vec![
            Value::BigInt(relid),
            Value::text("public"),
            Value::Text(alloc::borrow::Cow::Owned(name)),
            Value::BigInt(0),       // seq_scan
            Value::BigInt(0),       // seq_tup_read
            Value::BigInt(0),       // idx_scan
            Value::BigInt(0),       // idx_tup_fetch
            Value::BigInt(0),       // n_tup_ins
            Value::BigInt(0),       // n_tup_upd
            Value::BigInt(0),       // n_tup_del
            Value::BigInt(live_rows),
            Value::BigInt(0),       // n_dead_tup (vacuum tracks; 0 until 15.16)
            Value::Null,            // last_vacuum
            Value::Null,            // last_analyze
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
pub(crate) fn synth_pg_stat_database(_cat: &Catalog) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
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
        ColumnSchema::new("conflicts", DataType::BigInt, false),
        ColumnSchema::new("deadlocks", DataType::BigInt, false),
        ColumnSchema::new("temp_files", DataType::BigInt, false),
        ColumnSchema::new("temp_bytes", DataType::BigInt, false),
        ColumnSchema::new("blk_read_time", DataType::Float, false),
        ColumnSchema::new("blk_write_time", DataType::Float, false),
    ];
    // Single-row, single-database; everything reads as 0 until
    // per-counter wiring lands (the shape is stable so monitoring
    // queries parse).
    let rows = alloc::vec![Row::new(alloc::vec![
        Value::BigInt(16384),
        Value::text("spg"),
        Value::Int(0),         // numbackends — wired from server crate
        Value::BigInt(0),      // xact_commit
        Value::BigInt(0),      // xact_rollback
        Value::BigInt(0),      // blks_read
        Value::BigInt(0),      // blks_hit
        Value::BigInt(0),      // tup_returned
        Value::BigInt(0),      // tup_fetched
        Value::BigInt(0),      // tup_inserted
        Value::BigInt(0),      // tup_updated
        Value::BigInt(0),      // tup_deleted
        Value::BigInt(0),      // conflicts (PG: replication-conflict count)
        Value::BigInt(0),      // deadlocks (SPG single-writer; always 0)
        Value::BigInt(0),      // temp_files (spill; pending 19.15)
        Value::BigInt(0),      // temp_bytes
        Value::Float(0.0),     // blk_read_time
        Value::Float(0.0),     // blk_write_time
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
            Value::BigInt(10),    // subowner
            Value::Bool(sub.enabled),
            Value::text("[redacted]"), // subconninfo
            Value::Null,               // subslotname
            Value::text(pubs),
            Value::Bool(false),        // subbinary
            Value::Bool(false),        // substream
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
pub(crate) fn synth_pg_replication_slots(
    _cat: &Catalog,
) -> (Vec<ColumnSchema>, Vec<Row<'static>>) {
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
        .iter()
        .map(|(_name, def)| {
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
    for tname in cat.table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        // Uniqueness constraints — both PK and UNIQUE forms.
        for (_ci, uc) in t.schema().uniqueness_constraints.iter().enumerate() {
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
        for idx in t.indices() {
            if !idx.is_unique {
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
        // CHECK constraints.
        for (ci, _check) in t.schema().checks.iter().enumerate() {
            rows.push(Row::new(alloc::vec![
                Value::text("spg"),
                Value::text("public"),
                Value::text(alloc::format!("{tname}_check{ci}")),
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

/// v7.16.2 + v7.37.24 (24.8) — synthesise `pg_catalog.pg_class`.
/// Widened to cover the columns dashboards / monitoring tools
/// query (relkind, reltuples for size estimates, relnatts for
/// column count, relhasindex flag, relpersistence, relispartition
/// for partition awareness). PG18's pg_class has ~30 columns;
/// the subset here is "every column an external tool actually
/// reads against SPG" — additional columns land as we observe
/// new tools query them.
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
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    // PG starts user-relation OIDs above 16384.
    let mut oid: i64 = 16384;
    for tname in cat.table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        let schema_ref = t.schema();
        let relkind: &'static str = match &schema_ref.partition_role {
            Some(PartitionRole::Parent { .. }) => "p", // partitioned table
            _ => "r",                                   // regular table
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
            Value::BigInt(oid),
            Value::text(tname.clone()),
            Value::BigInt(2200), // public namespace
            Value::BigInt(0),    // reltype (composite type OID; SPG no composite)
            Value::BigInt(0),    // reloftype
            Value::BigInt(10),   // relowner — PG postgres superuser OID
            Value::BigInt(0),    // relam (table AM; 0 == default heap)
            Value::BigInt(oid),  // relfilenode shares oid in SPG (no separate fork)
            Value::BigInt(0),    // reltablespace (0 == default)
            Value::Int(relpages), // hot_bytes in 8 KiB PG-page units
            Value::Float(reltuples),
            Value::Int(0),       // relallvisible — visibility map lands in 15.17
            Value::BigInt(0),    // reltoastrelid (SPG no TOAST)
            Value::Bool(has_index),
            Value::Bool(false),  // relisshared
            Value::text("p"),    // relpersistence — 'p' permanent
            Value::text(relkind),
            Value::SmallInt(relnatts),
            Value::SmallInt(i16::try_from(has_checks).unwrap_or(i16::MAX)),
            Value::Bool(false),  // relhasrules — SPG has no rule system
            Value::Bool(has_triggers),
            Value::Bool(false),  // relhassubclass
            Value::Bool(false),  // relrowsecurity
            Value::Bool(false),  // relforcerowsecurity
            Value::Bool(true),   // relispopulated
            Value::text("d"),    // relreplident — 'd' default
            Value::Bool(is_partition),
        ]));
        oid = oid.saturating_add(1);
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
    ];
    let mut rows: Vec<Row<'static>> = Vec::new();
    let mut attrelid: i64 = 16384;
    for tname in cat.table_names() {
        let Some(t) = cat.get(&tname) else {
            attrelid = attrelid.saturating_add(1);
            continue;
        };
        for (i, col) in t.schema().columns.iter().enumerate() {
            #[allow(clippy::cast_possible_wrap)]
            let attnum = (i + 1) as i16;
            // PG: typlen — fixed-width width in bytes; -1 for var-length.
            let typlen: i16 = match col.ty {
                DataType::Bool => 1,
                DataType::SmallInt => 2,
                DataType::Int => 4,
                DataType::BigInt | DataType::Float | DataType::Timestamp | DataType::Timestamptz => 8,
                DataType::Date => 4,
                _ => -1,
            };
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
                Value::Int(-1),         // attstattarget — -1 = use system default
                Value::SmallInt(typlen),
                Value::SmallInt(attnum),
                Value::Int(attndims),
                Value::Int(-1),         // atttypmod — -1 = no modifier
                Value::Bool(typlen > 0 && typlen <= 8),
                Value::text(attstorage),
                Value::text(attalign),
                Value::Bool(!col.nullable),
                Value::Bool(has_default),
                Value::text(attidentity),
                Value::text(""),        // attgenerated — '' (not stored generated)
                Value::Bool(false),     // attisdropped
                Value::Bool(true),      // attislocal — true (not inherited)
                Value::Int(0),          // attinhcount
                Value::BigInt(0),       // attcollation — 0 (default)
            ]));
        }
        attrelid = attrelid.saturating_add(1);
    }
    (schema, rows)
}

/// PG type OID lookup for the SPG DataType set. Used by
/// `synth_pg_attribute`'s `atttypid` column.
fn pg_type_oid(ty: DataType) -> i64 {
    match ty {
        DataType::Bool => 16,
        DataType::Bytes => 17,
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
        "current_timestamp",
        "current_date",
        "current_time",
        "random",
        "gen_random_uuid",
        "uuid_generate_v4",
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
            Value::BigInt(11),         // pronamespace = pg_catalog
            Value::BigInt(10),         // proowner
            Value::BigInt(12),         // prolang = internal
            Value::Float(1.0),         // procost
            Value::Float(prorows),
            Value::BigInt(0),          // provariadic
            Value::text::<String>(kind.into()),
            Value::Bool(false),        // prosecdef
            Value::Bool(false),        // proleakproof
            Value::Bool(true),         // proisstrict
            Value::Bool(kind == "w"),  // proretset — window funcs return per-row sets
            Value::text::<String>(provolatile.into()),
            Value::text::<String>("s".into()), // proparallel = safe
            Value::SmallInt(i16::try_from(nargs.max(0)).unwrap_or(i16::MAX)),
            Value::SmallInt(0),        // pronargdefaults
            Value::BigInt(rettype),
            Value::text(argtypes),
            Value::text::<String>(name.into()), // prosrc
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
    for tname in cat.table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        let cols = &t.schema().columns;
        let col_name_at = |pos: usize| -> String {
            cols.get(pos)
                .map_or_else(|| alloc::format!("col{pos}"), |c| c.name.clone())
        };
        for (_ci, uc) in t.schema().uniqueness_constraints.iter().enumerate() {
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
                    let pname = parent.schema().columns.get(p).map_or_else(
                        || alloc::format!("col{p}"),
                        |c| c.name.clone(),
                    );
                    push(&fk.parent_table, pname, conname.clone());
                }
            }
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
                Value::text(alloc::format!(
                    "EXECUTE FUNCTION {}()",
                    trg.function
                )),
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
    for tname in cat.table_names() {
        let Some(t) = cat.get(&tname) else { continue };
        for (ci, clause) in t.schema().checks.iter().enumerate() {
            rows.push(Row::new(alloc::vec![
                Value::text("spg"),
                Value::text("public"),
                Value::text(alloc::format!("{tname}_check{ci}")),
                Value::text(clause.clone()),
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
    for (name, def) in cat.sequences() {
        let dt = match def.data_type {
            spg_storage::SequenceDataType::SmallInt => "smallint",
            spg_storage::SequenceDataType::Int => "integer",
            spg_storage::SequenceDataType::BigInt => "bigint",
        };
        rows.push(Row::new(alloc::vec![
            Value::text("spg"),
            Value::text("public"),
            Value::text(name.clone()),
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
        for (_ci, uc) in t.schema().uniqueness_constraints.iter().enumerate() {
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
            rows.push(Row::new(alloc::vec![
                Value::text(conname),
                Value::text(tname.clone()),
                Value::text(fk.parent_table.clone()),
                unique_name,
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
    // Sequence OIDs live in their own synthetic band, monotonically
    // assigned in name order (stable within a catalog snapshot).
    let mut seq_oid: i64 = 32768;
    for (_name, def) in cat.sequences() {
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
        seq_oid = seq_oid.saturating_add(1);
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
    let names = cat.table_names();
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
        // Helper to build PG's int2vector `conkey` body
        // (1-based, space-separated attnums).
        let conkey_vec = |positions: &[usize]| -> String {
            let mut s = String::new();
            for (i, p) in positions.iter().enumerate() {
                if i > 0 {
                    s.push(' ');
                }
                s.push_str(&alloc::format!("{}", p + 1));
            }
            s
        };
        // Uniqueness constraints.
        for uc in t.schema().uniqueness_constraints.iter() {
            let kind = if uc.is_primary_key { "p" } else { "u" };
            let conname = pg_unique_conname(t, uc, &tname);
            let conkey = conkey_vec(&uc.columns);
            let conkey_names: Vec<String> =
                uc.columns.iter().map(|&p| col_name_at(p)).collect();
            // Hybrid: PG's `conkey` is int2vector; expose the
            // int2vector form so the canonical PG query path
            // works, but include the names as a comma list in
            // the same string (`"1 2 [name1,name2]"`) so the
            // existing SPG dashboards that depended on the name
            // form still read sensibly. Pure-int form lands
            // when SPG ships proper int2vector support.
            let conkey_display =
                alloc::format!("{conkey} [{}]", conkey_names.join(","));
            rows.push(Row::new(alloc::vec![
                Value::BigInt(next_con_oid()),
                Value::text(conname),
                Value::BigInt(2200),
                Value::text::<String>(kind.into()),
                Value::Bool(false), // condeferrable
                Value::Bool(false), // condeferred
                Value::Bool(true),  // convalidated
                Value::BigInt(conrelid),
                Value::BigInt(0),   // contypid
                Value::BigInt(0),   // conindid — pending UC↔index plumb-through
                Value::BigInt(0),   // conparentid
                Value::BigInt(0),   // confrelid (not an FK)
                Value::text(" "),   // confupdtype
                Value::text(" "),   // confdeltype
                Value::text(" "),   // confmatchtype
                Value::Bool(true),  // conislocal
                Value::Int(0),      // coninhcount
                Value::Bool(true),  // connoinherit
                Value::text(conkey_display),
                Value::text(String::new()),
            ]));
        }
        // Single-column unique indices that don't have a UC entry.
        for idx in t.indices() {
            if !idx.is_unique {
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
            let conkey = conkey_vec(&positions);
            let col_name = col_name_at(idx.column_position);
            let conkey_display = alloc::format!("{conkey} [{col_name}]");
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
                .unwrap_or_else(|| pg_fk_conname(t, fk, &tname));
            let confrelid = by_table.get(&fk.parent_table).copied().unwrap_or(0);
            let conkey = conkey_vec(&fk.local_columns);
            let confkey = conkey_vec(&fk.parent_columns);
            let conkey_names: Vec<String> =
                fk.local_columns.iter().map(|&p| col_name_at(p)).collect();
            let confkey_names: Vec<String> = if let Some(parent) = cat.get(&fk.parent_table) {
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
            // confupdtype / confdeltype: 'a' no action, 'r' restrict,
            // 'c' cascade, 'n' set null, 'd' set default. SPG's
            // ForeignKey action enum already mirrors this.
            let upd_action = fk_action_char(&fk.on_update);
            let del_action = fk_action_char(&fk.on_delete);
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
                Value::text(alloc::format!("{conkey} [{}]", conkey_names.join(","))),
                Value::text(alloc::format!("{confkey} [{}]", confkey_names.join(","))),
            ]));
        }
        // v7.37 U5 — CHECK constraints (contype 'c'). Previously
        // omitted, so pg_constraint enumeration missed every CHECK.
        let check_names = pg_check_connames(t, &tname, &t.schema().checks);
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
fn fk_action_char(action: &spg_storage::FkAction) -> &'static str {
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
        ("server_version", "18.4 (spg)", "Preset Options"),
        ("server_encoding", "UTF8", "Client Connection Defaults"),
        ("client_encoding", "UTF8", "Client Connection Defaults"),
        ("DateStyle", "ISO, MDY", "Client Connection Defaults"),
        ("TimeZone", "UTC", "Client Connection Defaults"),
        ("IntervalStyle", "postgres", "Client Connection Defaults"),
        ("standard_conforming_strings", "on", "Compatibility"),
        ("integer_datetimes", "on", "Compatibility"),
        ("max_connections", "100", "Connections and Authentication"),
        // v7.37.17 (17.6 siblings) — parity with the SHOW <param>
        // PG-default fallbacks so `SELECT setting FROM pg_settings
        // WHERE name = 'lock_timeout'` and `SHOW lock_timeout`
        // return the same value (matches PG semantics + keeps
        // pgpool / pgbouncer / postgres_exporter probes happy).
        ("lock_timeout", "0", "Client Connection Defaults"),
        ("idle_in_transaction_session_timeout", "0", "Client Connection Defaults"),
        ("transaction_timeout", "0", "Client Connection Defaults"),
        ("statement_timeout", "0", "Client Connection Defaults"),
        ("client_min_messages", "notice", "Client Connection Defaults"),
        ("default_tablespace", "", "Client Connection Defaults"),
        ("default_table_access_method", "heap", "Client Connection Defaults"),
        ("row_security", "on", "Client Connection Defaults"),
        ("check_function_bodies", "on", "Client Connection Defaults"),
        ("xmloption", "content", "Client Connection Defaults"),
        ("work_mem", "4MB", "Resource Usage / Memory"),
        ("maintenance_work_mem", "64MB", "Resource Usage / Memory"),
        ("shared_buffers", "128MB", "Resource Usage / Memory"),
        ("effective_cache_size", "4GB", "Query Tuning / Planner Cost Constants"),
        ("search_path", "\"$user\", public", "Client Connection Defaults"),
        ("application_name", "", "Reporting and Logging"),
        ("default_transaction_isolation", "read committed", "Client Connection Defaults"),
    ];
    // v7.37.17 (17.6 siblings) — pg_settings row shape now honors
    // session-set overrides on the default row itself (not just as
    // extra rows), so `SELECT setting FROM pg_settings WHERE name =
    // 'lock_timeout'` returns whatever the session most recently
    // SET. Matches PG semantics.
    for &(name, val, cat) in defaults {
        let effective = engine
            .session_params
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| val.into());
        rows.push(Row::new(alloc::vec![
            Value::text::<String>(name.into()),
            Value::text(effective),
            Value::text::<String>(cat.into()),
        ]));
    }
    // Session-set params NOT in the canonical defaults get their
    // own rows under the Session category.
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
            let col_at = |pos: usize| -> String {
                t.schema()
                    .columns
                    .get(pos)
                    .map_or("?".into(), |c| c.name.clone())
            };
            let mut positions = alloc::vec![idx.column_position];
            positions.extend(idx.extra_column_positions.iter().copied());
            let cols = positions.iter().map(|&p| col_at(p)).collect::<Vec<_>>().join(", ");
            let unique_kw = if idx.is_unique { "UNIQUE " } else { "" };
            // Matches PG's pg_get_indexdef spelling (with `USING btree`).
            let indexdef = alloc::format!(
                "CREATE {unique_kw}INDEX {} ON public.{} USING btree ({})",
                idx.name,
                tname,
                cols
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
    let names = cat.table_names();
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
                Value::Bool(false), // indnullsnotdistinct — pending UniquenessConstraint plumb-through
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
            Expr::Case { operand, branches, else_branch } => {
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
                list.iter().for_each(|it| walk_expr(it, into));
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
        gs.iter().for_each(|g| walk_expr(g, into));
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
