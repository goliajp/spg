//! SHOW-statement executors — the MySQL/Postgres-compatible `SHOW`
//! family (TABLES / CREATE TABLE / INDEXES / STATUS / VARIABLES /
//! PROCESSLIST / DATABASES / COLUMNS). Lifted out of `lib.rs` (v7.32
//! engine modularisation). Each is dispatched from the statement
//! executor and returns a synthesised `Rows` result.

use alloc::string::String;
use alloc::vec::Vec;

use spg_storage::{ColumnSchema, DataType, Row, StorageError, Value};

use crate::{Engine, EngineError, QueryResult};

impl Engine {
    /// `SHOW TABLES` — one row per table in the active catalog.
    /// Column name is `name` so result-set consumers can downstream
    /// `SELECT name FROM ...` style logic if needed.
    pub(crate) fn exec_show_tables(&self) -> QueryResult {
        let columns = alloc::vec![ColumnSchema::new("name", DataType::Text, false)];
        let rows: Vec<Row> = self
            .active_catalog()
            .table_names()
            .into_iter()
            .map(|n| Row::new(alloc::vec![Value::Text(n)]))
            .collect();
        QueryResult::Rows { columns, rows }
    }

    /// v7.17.0 Phase 3.P0-59 — `SHOW CREATE TABLE <t>`. Synthesise
    /// a minimal MySQL-flavoured CREATE TABLE DDL from the
    /// catalog's TableSchema so mysqldump round-trips load against
    /// SPG without splitting init scripts.
    pub(crate) fn exec_show_create_table(&self, name: &str) -> Result<QueryResult, EngineError> {
        let t = self.active_catalog().get(name).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound { name: name.into() })
        })?;
        let cols: Vec<String> = t
            .schema()
            .columns
            .iter()
            .map(|c| {
                let ty = render_data_type(c.ty);
                let nullable = if c.nullable { "" } else { " NOT NULL" };
                alloc::format!("  `{}` {}{}", c.name, ty, nullable)
            })
            .collect();
        let mut body = cols.join(",\n");
        // Append UNIQUE / PRIMARY KEY clauses.
        for uc in &t.schema().uniqueness_constraints {
            let col_names: Vec<String> = uc
                .columns
                .iter()
                .map(|&p| {
                    t.schema().columns.get(p).map_or_else(
                        || alloc::format!("col{p}"),
                        |c| alloc::format!("`{}`", c.name),
                    )
                })
                .collect();
            let kw = if uc.is_primary_key {
                "PRIMARY KEY"
            } else {
                "UNIQUE KEY"
            };
            body.push_str(",\n  ");
            body.push_str(&alloc::format!("{kw} ({})", col_names.join(", ")));
        }
        // Foreign keys.
        for fk in &t.schema().foreign_keys {
            let local: Vec<String> = fk
                .local_columns
                .iter()
                .map(|&p| {
                    t.schema().columns.get(p).map_or_else(
                        || alloc::format!("col{p}"),
                        |c| alloc::format!("`{}`", c.name),
                    )
                })
                .collect();
            let parent_cols: Vec<String> =
                if let Some(parent) = self.active_catalog().get(&fk.parent_table) {
                    fk.parent_columns
                        .iter()
                        .map(|&p| {
                            parent.schema().columns.get(p).map_or_else(
                                || alloc::format!("col{p}"),
                                |c| alloc::format!("`{}`", c.name),
                            )
                        })
                        .collect()
                } else {
                    fk.parent_columns
                        .iter()
                        .map(|p| alloc::format!("col{p}"))
                        .collect()
                };
            body.push_str(",\n  ");
            body.push_str(&alloc::format!(
                "FOREIGN KEY ({}) REFERENCES `{}` ({})",
                local.join(", "),
                fk.parent_table,
                parent_cols.join(", ")
            ));
        }
        let ddl = alloc::format!(
            "CREATE TABLE `{}` (\n{}\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4",
            name,
            body
        );
        let columns = alloc::vec![
            ColumnSchema::new("Table", DataType::Text, false),
            ColumnSchema::new("Create Table", DataType::Text, false),
        ];
        let rows = alloc::vec![Row::new(alloc::vec![
            Value::Text(name.into()),
            Value::Text(ddl),
        ])];
        Ok(QueryResult::Rows { columns, rows })
    }

    /// v7.17.0 Phase 3.P0-60 — `SHOW INDEXES FROM <t>`. MySQL
    /// surface returns one row per (index × column) with 14
    /// columns; v7.17 ships the columns admin probes actually
    /// filter on: Table, Non_unique, Key_name, Seq_in_index,
    /// Column_name, Null, Index_type.
    pub(crate) fn exec_show_indexes(&self, name: &str) -> Result<QueryResult, EngineError> {
        let t = self.active_catalog().get(name).ok_or_else(|| {
            EngineError::Storage(StorageError::TableNotFound { name: name.into() })
        })?;
        let columns = alloc::vec![
            ColumnSchema::new("Table", DataType::Text, false),
            ColumnSchema::new("Non_unique", DataType::Int, false),
            ColumnSchema::new("Key_name", DataType::Text, false),
            ColumnSchema::new("Seq_in_index", DataType::Int, false),
            ColumnSchema::new("Column_name", DataType::Text, false),
            ColumnSchema::new("Null", DataType::Text, false),
            ColumnSchema::new("Index_type", DataType::Text, false),
        ];
        let mut rows: Vec<Row> = Vec::new();
        for idx in t.indices() {
            let col = t
                .schema()
                .columns
                .get(idx.column_position)
                .map_or("?".into(), |c| c.name.clone());
            let nullable = t
                .schema()
                .columns
                .get(idx.column_position)
                .map_or(true, |c| c.nullable);
            rows.push(Row::new(alloc::vec![
                Value::Text(name.into()),
                Value::Int(i32::from(!idx.is_unique)),
                Value::Text(idx.name.clone()),
                Value::Int(1),
                Value::Text(col),
                Value::Text(if nullable {
                    "YES".into()
                } else {
                    String::new()
                }),
                Value::Text("BTREE".into()),
            ]));
        }
        Ok(QueryResult::Rows { columns, rows })
    }

    /// v7.17.0 Phase 3.P0-61 — `SHOW STATUS`. Returns canonical
    /// MySQL server-status counters (2-column `(Variable_name,
    /// Value)`).
    pub(crate) fn exec_show_status(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("Variable_name", DataType::Text, false),
            ColumnSchema::new("Value", DataType::Text, false),
        ];
        let pairs: &[(&str, &str)] = &[
            ("Uptime", "0"),
            ("Threads_connected", "1"),
            ("Threads_running", "1"),
            ("Questions", "0"),
            ("Slow_queries", "0"),
            ("Opened_tables", "0"),
            ("Innodb_buffer_pool_pages_total", "0"),
        ];
        let rows: Vec<Row> = pairs
            .iter()
            .map(|(k, v)| {
                Row::new(alloc::vec![
                    Value::Text((*k).into()),
                    Value::Text((*v).into())
                ])
            })
            .collect();
        QueryResult::Rows { columns, rows }
    }

    /// v7.17.0 Phase 3.P0-61 — `SHOW VARIABLES`. Returns server-side
    /// variables MySQL/MariaDB clients probe at connect time.
    pub(crate) fn exec_show_variables(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("Variable_name", DataType::Text, false),
            ColumnSchema::new("Value", DataType::Text, false),
        ];
        let mut rows: Vec<Row> = Vec::new();
        let canonical: &[(&str, &str)] = &[
            ("version", "8.0.35-spg"),
            ("version_comment", "SPG dual-stack engine"),
            ("character_set_server", "utf8mb4"),
            ("collation_server", "utf8mb4_0900_ai_ci"),
            ("max_allowed_packet", "67108864"),
            ("autocommit", "ON"),
            ("sql_mode", "STRICT_TRANS_TABLES,NO_ENGINE_SUBSTITUTION"),
            ("time_zone", "SYSTEM"),
            ("transaction_isolation", "REPEATABLE-READ"),
        ];
        for &(k, v) in canonical {
            rows.push(Row::new(alloc::vec![
                Value::Text(k.into()),
                Value::Text(v.into()),
            ]));
        }
        // Session-set parameters surface here too.
        for (k, v) in &self.session_params {
            if !canonical.iter().any(|(n, _)| (*n).eq_ignore_ascii_case(k)) {
                rows.push(Row::new(alloc::vec![
                    Value::Text(k.clone()),
                    Value::Text(v.clone()),
                ]));
            }
        }
        QueryResult::Rows { columns, rows }
    }

    /// v7.17.0 Phase 3.P0-62 — `SHOW PROCESSLIST`. SPG is
    /// single-process so the surface returns one synthetic row
    /// describing the current connection (Id, User, Host, db,
    /// Command, Time, State, Info).
    pub(crate) fn exec_show_processlist(&self) -> QueryResult {
        let columns = alloc::vec![
            ColumnSchema::new("Id", DataType::Int, false),
            ColumnSchema::new("User", DataType::Text, false),
            ColumnSchema::new("Host", DataType::Text, false),
            ColumnSchema::new("db", DataType::Text, true),
            ColumnSchema::new("Command", DataType::Text, false),
            ColumnSchema::new("Time", DataType::Int, false),
            ColumnSchema::new("State", DataType::Text, true),
            ColumnSchema::new("Info", DataType::Text, true),
        ];
        let rows = alloc::vec![Row::new(alloc::vec![
            Value::Int(1),
            Value::Text("postgres".into()),
            Value::Text("localhost".into()),
            Value::Text("postgres".into()),
            Value::Text("Query".into()),
            Value::Int(0),
            Value::Text("executing".into()),
            Value::Text("SHOW PROCESSLIST".into()),
        ])];
        QueryResult::Rows { columns, rows }
    }

    /// v7.17.0 Phase 3.P0-58 — `SHOW DATABASES` / `SHOW SCHEMAS`.
    /// SPG is single-database so the result is the canonical MySQL
    /// set every mysql/MariaDB client expects at connect time:
    /// `information_schema`, `mysql`, `performance_schema`, `sys`,
    /// plus a `postgres` slot so dual-stack callers find their
    /// PG-compatible database too.
    pub(crate) fn exec_show_databases(&self) -> QueryResult {
        let columns = alloc::vec![ColumnSchema::new("Database", DataType::Text, false)];
        let names = [
            "information_schema",
            "mysql",
            "performance_schema",
            "sys",
            "postgres",
        ];
        let rows: Vec<Row> = names
            .iter()
            .map(|n| Row::new(alloc::vec![Value::Text((*n).into())]))
            .collect();
        QueryResult::Rows { columns, rows }
    }

    /// `SHOW COLUMNS FROM <table>` — one row per column with the
    /// declared name, SQL type rendering, and nullability flag.
    pub(crate) fn exec_show_columns(&self, table_name: &str) -> Result<QueryResult, EngineError> {
        let table =
            self.active_catalog()
                .get(table_name)
                .ok_or_else(|| StorageError::TableNotFound {
                    name: table_name.into(),
                })?;
        let columns = alloc::vec![
            ColumnSchema::new("name", DataType::Text, false),
            ColumnSchema::new("type", DataType::Text, false),
            ColumnSchema::new("nullable", DataType::Bool, false),
        ];
        let rows: Vec<Row> = table
            .schema()
            .columns
            .iter()
            .map(|c| {
                Row::new(alloc::vec![
                    Value::Text(c.name.clone()),
                    Value::Text(alloc::format!("{}", c.ty)),
                    Value::Bool(c.nullable),
                ])
            })
            .collect();
        Ok(QueryResult::Rows { columns, rows })
    }
}

// ---- CREATE TABLE / data-type DDL rendering (lib.rs split 14) ----

/// v6.5.4 — synthesise a `CREATE TABLE` statement from catalog
/// state. Round-trips through `Engine::execute` to recreate the
/// same schema (sans data + indexes — indexes are emitted as a
/// separate `CREATE INDEX` chain in `spg_database_ddl`).
pub(crate) fn render_create_table(name: &str, columns: &[ColumnSchema]) -> String {
    let mut out = alloc::format!("CREATE TABLE {name} (");
    for (i, col) in columns.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&col.name);
        out.push(' ');
        out.push_str(&render_data_type(col.ty));
        if !col.nullable {
            out.push_str(" NOT NULL");
        }
        if col.auto_increment {
            out.push_str(" AUTO_INCREMENT");
        }
    }
    out.push(')');
    out
}

fn render_data_type(ty: DataType) -> String {
    match ty {
        DataType::SmallInt => "SMALLINT".into(),
        DataType::Int => "INT".into(),
        DataType::BigInt => "BIGINT".into(),
        DataType::Float => "FLOAT".into(),
        DataType::Text => "TEXT".into(),
        DataType::Varchar(n) => alloc::format!("VARCHAR({n})"),
        DataType::Char(n) => alloc::format!("CHAR({n})"),
        DataType::Bool => "BOOL".into(),
        DataType::Vector { dim, encoding } => match encoding {
            spg_storage::VecEncoding::F32 => alloc::format!("VECTOR({dim})"),
            spg_storage::VecEncoding::Sq8 => alloc::format!("VECTOR({dim}) USING SQ8"),
            spg_storage::VecEncoding::F16 => alloc::format!("VECTOR({dim}) USING HALF"),
        },
        DataType::Numeric { precision, scale } => {
            alloc::format!("NUMERIC({precision},{scale})")
        }
        DataType::Date => "DATE".into(),
        DataType::Timestamp => "TIMESTAMP".into(),
        DataType::Interval => "INTERVAL".into(),
        DataType::Json => "JSON".into(),
        DataType::Jsonb => "JSONB".into(),
        DataType::Timestamptz => "TIMESTAMPTZ".into(),
        DataType::Bytes => "BYTEA".into(),
        DataType::TextArray => "TEXT[]".into(),
        DataType::IntArray => "INT[]".into(),
        DataType::BigIntArray => "BIGINT[]".into(),
        DataType::TsVector => "TSVECTOR".into(),
        DataType::TsQuery => "TSQUERY".into(),
        DataType::Uuid => "UUID".into(),
        DataType::Time => "TIME".into(),
        DataType::Year => "YEAR".into(),
        DataType::TimeTz => "TIMETZ".into(),
        DataType::Money => "MONEY".into(),
        DataType::Range(k) => k.keyword().into(),
        DataType::Hstore => "HSTORE".into(),
        DataType::IntArray2D => "INT[][]".into(),
        DataType::BigIntArray2D => "BIGINT[][]".into(),
        DataType::TextArray2D => "TEXT[][]".into(),
        DataType::IntervalArray => "INTERVAL[]".into(),
        DataType::BoolArray => "BOOL[]".into(),
        DataType::SmallIntArray => "SMALLINT[]".into(),
        DataType::FloatArray => "FLOAT[]".into(),
        DataType::NumericArray => "NUMERIC[]".into(),
        DataType::DateArray => "DATE[]".into(),
        DataType::TimestampArray => "TIMESTAMP[]".into(),
        DataType::TimestamptzArray => "TIMESTAMPTZ[]".into(),
        DataType::UuidArray => "UUID[]".into(),
        DataType::JsonArray => "JSON[]".into(),
        DataType::JsonbArray => "JSONB[]".into(),
        DataType::BytesArray => "BYTEA[]".into(),
        DataType::VarcharArray => "VARCHAR[]".into(),
        DataType::CharArray => "CHAR[]".into(),
        DataType::Point => "POINT".into(),
        DataType::Lseg => "LSEG".into(),
        DataType::Path => "PATH".into(),
        DataType::PgBox => "BOX".into(),
        DataType::Polygon => "POLYGON".into(),
        DataType::Line => "LINE".into(),
        DataType::Circle => "CIRCLE".into(),
        DataType::Inet => "INET".into(),
        DataType::Cidr => "CIDR".into(),
        DataType::Macaddr => "MACADDR".into(),
        DataType::Macaddr8 => "MACADDR8".into(),
        DataType::Bit => "BIT".into(),
        DataType::BitVarying => "VARBIT".into(),
        DataType::Xml => "XML".into(),
        DataType::Char1 => "\"char\"".into(),
        DataType::MoneyArray => "MONEY[]".into(),
        DataType::Multirange(k) => match k {
            spg_storage::RangeKind::Int4 => "INT4MULTIRANGE".into(),
            spg_storage::RangeKind::Int8 => "INT8MULTIRANGE".into(),
            spg_storage::RangeKind::Num => "NUMMULTIRANGE".into(),
            spg_storage::RangeKind::Ts => "TSMULTIRANGE".into(),
            spg_storage::RangeKind::TsTz => "TSTZMULTIRANGE".into(),
            spg_storage::RangeKind::Date => "DATEMULTIRANGE".into(),
        },
    }
}
