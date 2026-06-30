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
        for (ci, uc) in t.schema().uniqueness_constraints.iter().enumerate() {
            let conname = if uc.is_primary_key {
                alloc::format!("{tname}_pkey")
            } else {
                alloc::format!("{tname}_uniq{ci}")
            };
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
        for (fi, fk) in t.schema().foreign_keys.iter().enumerate() {
            let conname = fk
                .name
                .clone()
                .unwrap_or_else(|| alloc::format!("{tname}_fk{fi}"));
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
            Value::Int(0),       // relpages (no shared-buffer accounting)
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
        for (ci, uc) in t.schema().uniqueness_constraints.iter().enumerate() {
            let kind = if uc.is_primary_key { "p" } else { "u" };
            let conname = if uc.is_primary_key {
                alloc::format!("{}_pkey", tname)
            } else {
                alloc::format!("{}_uniq{ci}", tname)
            };
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
        for (fi, fk) in t.schema().foreign_keys.iter().enumerate() {
            let conname = fk
                .name
                .clone()
                .unwrap_or_else(|| alloc::format!("{}_fk{fi}", tname));
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
