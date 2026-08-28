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
        let rows: Vec<Row<'static>> = self
            .active_catalog()
            .visible_table_names()
            .into_iter()
            .map(|n| Row::new(alloc::vec![Value::text(n)]))
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
        // v7.39 (round 358, M16) — MySQL's own rendering. Measured on
        // MariaDB 11 against the same schema; the shape was missing more
        // than formatting:
        //   * every column DEFAULT was dropped — `b BIGINT DEFAULT 7`
        //     came back with no default at all;
        //   * every secondary index was dropped;
        //   * AUTO_INCREMENT was dropped from the identity column.
        // mysqldump round-trips a schema through exactly this statement,
        // so a client lost its defaults and its indexes without a word.
        let cols: Vec<String> = t
            .schema()
            .columns
            .iter()
            .map(|c| {
                let ty = render_mysql_type(c);
                let nullable = if c.nullable { "" } else { " NOT NULL" };
                let auto = if c.auto_increment {
                    " AUTO_INCREMENT"
                } else {
                    ""
                };
                // MariaDB prints the declared default, and `DEFAULT NULL`
                // for a nullable column that has none; a NOT NULL column
                // without one gets nothing.
                let default = match (&c.default_text, c.nullable, c.auto_increment) {
                    (Some(d), _, _) => alloc::format!(" DEFAULT {}", mysql_default_text(d)),
                    (None, true, false) => " DEFAULT NULL".into(),
                    _ => String::new(),
                };
                alloc::format!("  `{}` {}{}{}{}", c.name, ty, nullable, default, auto)
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
        // v7.39 (round 358, M16) — the secondary indexes, which were
        // absent entirely. MariaDB's order is PRIMARY KEY, then UNIQUE
        // KEY, then KEY; the uniqueness constraints above already
        // emitted the first two, so the plain ones follow here.
        // MariaDB's order is PRIMARY KEY, then UNIQUE KEY, then KEY, so
        // the unique ones go out first.
        for unique_pass in [true, false] {
            for idx in t.indices() {
                if idx.is_unique != unique_pass {
                    continue;
                }
                let col = t.schema().columns.get(idx.column_position).map_or_else(
                    || alloc::format!("col{}", idx.column_position),
                    |c| alloc::format!("`{}`", c.name),
                );
                // An index that merely backs a declared UNIQUE constraint
                // was already printed by the loop above.
                let backs_constraint = t
                    .schema()
                    .uniqueness_constraints
                    .iter()
                    .any(|uc| uc.columns == [idx.column_position]);
                if backs_constraint {
                    continue;
                }
                let kw = if idx.is_unique { "UNIQUE KEY" } else { "KEY" };
                body.push_str(",\n  ");
                body.push_str(&alloc::format!("{kw} `{}` ({col})", idx.name));
            }
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
        // v7.39 (round 358, M16) — the AUTO_INCREMENT table option, which
        // is the next value the table would hand out (MariaDB prints
        // `AUTO_INCREMENT=3` after two rows).
        let auto_opt = t
            .schema()
            .columns
            .iter()
            .position(|c| c.auto_increment)
            .and_then(|i| t.next_auto_value(i))
            .map_or_else(String::new, |n| alloc::format!(" AUTO_INCREMENT={n}"));
        let ddl = alloc::format!(
            "CREATE TABLE `{}` (\n{}\n) ENGINE=InnoDB{} DEFAULT CHARSET=utf8mb4",
            name,
            body,
            auto_opt
        );
        let columns = alloc::vec![
            ColumnSchema::new("Table", DataType::Text, false),
            ColumnSchema::new("Create Table", DataType::Text, false),
        ];
        let rows = alloc::vec![Row::new(alloc::vec![
            Value::text::<String>(name.into()),
            Value::text(ddl),
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
        let mut rows: Vec<Row<'static>> = Vec::new();
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
                Value::text::<String>(name.into()),
                Value::Int(i32::from(!idx.is_unique)),
                Value::text(idx.name.clone()),
                Value::Int(1),
                Value::text(col),
                Value::text(if nullable {
                    "YES".into()
                } else {
                    String::new()
                }),
                Value::text("BTREE"),
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
        let rows: Vec<Row<'static>> = pairs
            .iter()
            .map(|(k, v)| {
                Row::new(alloc::vec![
                    Value::text::<String>((*k).into()),
                    Value::text::<String>((*v).into())
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
        let mut rows: Vec<Row<'static>> = Vec::new();
        let canonical: &[(&str, &str)] = &[
            ("version", crate::MYSQL_SERVER_VERSION),
            ("version_comment", crate::MYSQL_VERSION_COMMENT),
            // v7.39 — `SHOW VARIABLES LIKE 'collation%'` listed ONE of
            // MySQL's three and `LIKE 'character_set%'` one of its
            // eight, while `@@collation_connection` answered on the
            // other surface. Same disagreement between two surfaces this
            // release keeps finding, in the inventory rather than in a
            // value: a client that enumerates gets a different world
            // from one that asks by name.
            //
            // The loop below prefers the session's own value, so these
            // follow `SET NAMES` exactly as the `@@` path does — except
            // the two database-scoped names, which MySQL does not scope
            // to the session.
            // v7.39.2 — the inventory carried no entry for it at all,
            // while the `@@` surface answered. Same one-question-two-
            // surfaces shape as the collation names above.
            (
                "lower_case_table_names",
                if self.folds_relation_names() {
                    "1"
                } else {
                    "0"
                },
            ),
            ("character_set_client", "utf8mb4"),
            ("character_set_connection", "utf8mb4"),
            ("character_set_results", "utf8mb4"),
            ("character_set_database", "utf8mb4"),
            ("character_set_server", "utf8mb4"),
            // See the `@@` surface for why these two and not the third:
            // identifiers here really are utf8mb4 (MySQL's own answer is
            // utf8mb3 because it truncates a four-byte one), names are
            // used as bytes, and `character_sets_dir` names a directory
            // SPG does not have.
            ("character_set_system", "utf8mb4"),
            ("character_set_filesystem", "binary"),
            (
                "collation_connection",
                crate::collate::MYSQL_DEFAULT_CONNECTION_COLLATION,
            ),
            (
                "collation_database",
                crate::collate::MYSQL_DEFAULT_CONNECTION_COLLATION,
            ),
            (
                "collation_server",
                crate::collate::MYSQL_DEFAULT_CONNECTION_COLLATION,
            ),
            ("max_allowed_packet", "67108864"),
            ("autocommit", "ON"),
            // v7.39 (round 470) — the session's own value when it set one;
            // a client that reads sql_mode back after setting it was told
            // the default regardless.
            ("sql_mode", crate::MYSQL_DEFAULT_SQL_MODE),
            ("time_zone", "SYSTEM"),
            // v7.39 — the LIVE level, via the one function all three
            // surfaces now ask. This held the literal `REPEATABLE-READ`
            // (MySQL's default) while the engine ran read committed and
            // `@@transaction_isolation` held a second literal saying so;
            // `current_setting()` held a third on the PG side. One
            // question, three hard-coded answers, two of them wrong —
            // and the one a client is most likely to trust is the one
            // that promises a snapshot it does not have.
            //
            // Whether the MySQL dialect should DEFAULT to REPEATABLE
            // READ (MySQL 9.7.2 does; SPG implements RR — `transaction.rs`
            // caches the BEGIN snapshot for RR/SERIALIZABLE) is a
            // behavioural change with its own verification, tracked
            // separately. Reporting truthfully does not wait on it.
            (
                "transaction_isolation",
                self.current_isolation_level().as_mysql_str(),
            ),
        ];
        for &(k, v) in canonical {
            // v7.39 — a canonical name used to report its DEFAULT here
            // forever, because this pushed the constant and the
            // session-parameter loop below skips any name already in this
            // table. So `SET sql_mode = 'NO_ZERO_DATE'` was honoured by
            // `@@sql_mode` and ignored by `SHOW VARIABLES`, which went on
            // naming the default — and the same held for every other
            // canonical name, `SET NAMES` included.
            //
            // Measured on MySQL 9.7.2: after `SET sql_mode='NO_ZERO_DATE'`
            // both surfaces answer `NO_ZERO_DATE`.
            //
            // `transaction_isolation` and `transaction_read_only` are
            // excluded because their value here is computed live from the
            // engine, not stored: a stale copy in the parameter map would
            // shadow the truth rather than reveal it.
            let live = matches!(k, "transaction_isolation" | "transaction_read_only");
            let value = if live {
                v
            } else {
                self.session_params.get(k).map_or(v, String::as_str)
            };
            rows.push(Row::new(alloc::vec![
                Value::text::<String>(k.into()),
                Value::text::<String>(value.into()),
            ]));
        }
        // Session-set parameters surface here too.
        for (k, v) in &self.session_params {
            if !canonical.iter().any(|(n, _)| (*n).eq_ignore_ascii_case(k)) {
                rows.push(Row::new(alloc::vec![
                    Value::text(k.clone()),
                    Value::text(v.clone()),
                ]));
            }
        }
        QueryResult::Rows { columns, rows }
    }

    /// r1067 — `SHOW VARIABLES LIKE 'pattern'`: the full listing
    /// filtered by MySQL LIKE semantics on Variable_name (`%` any run,
    /// `_` any one char, case-insensitive — MySQL system variable
    /// names compare caselessly).
    pub(crate) fn exec_show_variables_like(&self, pattern: &str) -> QueryResult {
        let QueryResult::Rows { columns, rows } = self.exec_show_variables() else {
            unreachable!("exec_show_variables always answers Rows");
        };
        let rows = rows
            .into_iter()
            .filter(|r| match r.values.first() {
                Some(Value::Text(name)) => mysql_like_ci(name, pattern),
                _ => false,
            })
            .collect();
        QueryResult::Rows { columns, rows }
    }

    /// v7.17.0 Phase 3.P0-62 — `SHOW PROCESSLIST`.
    ///
    /// v7.39 (round 317, V36) — the LIVE connections, read through the
    /// same activity provider `spg_stat_activity` uses. It used to be a
    /// single hardcoded row (`Id` 1, user "postgres", Info
    /// "SHOW PROCESSLIST") no matter how many clients were attached, so
    /// the one MySQL surface an operator reaches for to find a runaway
    /// connection could never show one. Measured against MariaDB 11: `Id`
    /// is the connection's own `CONNECTION_ID()`, an idle connection
    /// reports Command `Sleep` with NULL `Info`, and a busy one reports
    /// `Query` with the statement text.
    ///
    /// A host with no provider registered (the embedded path, which has
    /// exactly one logical connection and no registry) keeps the single
    /// self row.
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
        let Some(provider) = self.activity_provider else {
            let rows = alloc::vec![Row::new(alloc::vec![
                Value::Int(1),
                Value::text("postgres"),
                Value::text("localhost"),
                Value::text("postgres"),
                Value::text::<String>("Query".into()),
                Value::Int(0),
                Value::text::<String>("executing".into()),
                Value::text::<String>("SHOW PROCESSLIST".into()),
            ])];
            return QueryResult::Rows { columns, rows };
        };
        let rows: Vec<Row<'static>> = provider()
            .into_iter()
            .map(|r| {
                let idle = r.current_sql.is_empty();
                Row::new(alloc::vec![
                    Value::Int(i32::try_from(r.pid).unwrap_or(i32::MAX)),
                    Value::text(r.user),
                    // Measured on MariaDB 11: `Host` is `addr:port` for a
                    // TCP client and `localhost` for a unix socket. SPG
                    // reported a hardcoded "localhost" for every row.
                    if r.client_addr.is_empty() {
                        Value::text("localhost")
                    } else {
                        Value::text(alloc::format!("{}:{}", r.client_addr, r.client_port))
                    },
                    // …and `db` is the database that connection selected,
                    // NULL when it selected none. This was hardcoded
                    // "postgres".
                    if r.database.is_empty() {
                        Value::Null
                    } else {
                        Value::text(r.database)
                    },
                    Value::text(if idle { "Sleep" } else { "Query" }),
                    Value::Int(i32::try_from(r.elapsed_us / 1_000_000).unwrap_or(i32::MAX)),
                    if r.wait_event.is_empty() {
                        Value::Null
                    } else {
                        Value::text(r.wait_event)
                    },
                    if idle {
                        Value::Null
                    } else {
                        Value::text(r.current_sql)
                    },
                ])
            })
            .collect();
        QueryResult::Rows { columns, rows }
    }

    /// v7.39 (round 318, V51) — MySQL `KILL [CONNECTION | QUERY] <id>`.
    /// The statement did not exist at all before this, so a MySQL client
    /// (or a DBA following MariaDB's own advice for dropping a runaway
    /// connection) got a syntax error.
    ///
    /// MariaDB 11 measured: an id no connection carries is
    /// `ERROR 1094 (HY000) Unknown thread id: N`; killing your own
    /// connection succeeds and then reports
    /// `ERROR 1927 (70100) Connection was killed` on the way out.
    /// The host registry answers whether the id is live — the engine has
    /// no connection registry of its own, and without a host hook
    /// (embedded) there are no connections, so every id is unknown.
    pub(crate) fn exec_kill(
        &mut self,
        query_only: bool,
        id: &spg_sql::ast::Expr,
    ) -> Result<QueryResult, crate::EngineError> {
        let target = {
            let empty: Vec<ColumnSchema> = alloc::vec::Vec::new();
            let ctx = self.ev_ctx(&empty, None);
            let dummy = Row::new(alloc::vec::Vec::new());
            crate::eval::eval_expr(id, &dummy, &ctx).map_err(crate::EngineError::Eval)?
        };
        let target = match target {
            Value::Int(n) => i64::from(n),
            Value::BigInt(n) => n,
            Value::SmallInt(n) => i64::from(n),
            other => {
                return Err(crate::EngineError::Unsupported(alloc::format!(
                    "KILL expects a connection id, got {}",
                    crate::conversions::pg_type_name_for_error_opt(other.data_type())
                )));
            }
        };
        let Ok(target) = u32::try_from(target) else {
            return Err(crate::EngineError::UnknownThreadId(u32::MAX));
        };
        let signalled = self
            .backend_signal_fn
            .is_some_and(|f| f(target, !query_only));
        if !signalled {
            return Err(crate::EngineError::UnknownThreadId(target));
        }
        // Killing your own connection works — and then you are told so.
        // Only the CONNECTION form ends the session; KILL QUERY on
        // yourself just cancels the statement you are already past.
        if !query_only && self.backend_pid_fn.is_some_and(|f| f() == target) {
            return Err(crate::EngineError::ConnectionKilled);
        }
        Ok(QueryResult::CommandOk {
            affected: 0,
            modified_catalog: false,
        })
    }

    /// v7.17.0 Phase 3.P0-58 — `SHOW DATABASES` / `SHOW SCHEMAS`.
    /// SPG is single-database so the result is the canonical MySQL
    /// set every mysql/MariaDB client expects at connect time:
    /// `information_schema`, `mysql`, `performance_schema`, `sys`,
    /// plus a `postgres` slot so dual-stack callers find their
    /// PG-compatible database too.
    pub(crate) fn exec_show_databases(&self) -> QueryResult {
        let columns = alloc::vec![ColumnSchema::new("Database", DataType::Text, false)];
        // v7.39.2 — the REAL databases, from the same place `pg_database`
        // reads them, plus MySQL's system schemas.
        //
        // It was a fixed list, so a database this server had just been
        // asked to create was absent from it while `pg_database` listed
        // it — the two wires answering the same question differently.
        // That is the defect sentori reported against 7.38.18 for
        // `pg_database` itself ("a migration tool's 'does this database
        // exist', and a backup script that enumerates all"); the MySQL
        // spelling of the question was never given the same treatment.
        //
        // The names are aliases onto one database (see `CREATE
        // DATABASE`), which is what makes listing them honest rather
        // than a promise of isolation.
        let rows: Vec<Row<'static>> = self
            .listed_database_names()
            .into_iter()
            .map(|n| Row::new(alloc::vec![Value::text(n)]))
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
        let rows: Vec<Row<'static>> = table
            .schema()
            .columns
            .iter()
            .map(|c| {
                Row::new(alloc::vec![
                    Value::text(c.name.clone()),
                    // v7.39 (round 471) — a MySQL session reads the DECLARED
                    // type, the same rendering SHOW CREATE and
                    // information_schema already give: `tinyint(4)`,
                    // `bigint(20) unsigned`. This reported the storage tag
                    // instead (`SMALLINT`, `NUMERIC(20)`), which was never
                    // what MariaDB says and became visibly so once BIGINT
                    // UNSIGNED moved its storage to Numeric.
                    Value::text(if self.speaks_mysql {
                        render_mysql_type(c)
                    } else {
                        alloc::format!("{}", c.ty)
                    }),
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

/// v7.39 (round 389, epic P4a) — the MySQL integer-family base name a
/// column reports, from its declared width annotation (TINYINT / MEDIUMINT,
/// and the widened SMALLINT / INT UNSIGNED) or, absent one, its storage
/// tag. `None` for a non-integer column. Shared by `render_mysql_type`
/// (the `column_type` form) and `mysql_data_type_text` (the bare
/// `data_type` form).
pub(crate) fn mysql_int_base_name(
    ty: DataType,
    width: Option<spg_storage::MysqlIntWidth>,
) -> Option<&'static str> {
    use spg_storage::MysqlIntWidth as W;
    match width {
        Some(W::Tiny) => Some("tinyint"),
        Some(W::Small) => Some("smallint"),
        Some(W::Medium) => Some("mediumint"),
        Some(W::Int) => Some("int"),
        // v7.39 (round 471, epic P4b) — BIGINT UNSIGNED stores as Numeric;
        // the marker is what keeps it reporting as a bigint.
        Some(W::Big) => Some("bigint"),
        None => match ty {
            DataType::SmallInt => Some("smallint"),
            DataType::Int => Some("int"),
            DataType::BigInt => Some("bigint"),
            _ => None,
        },
    }
}

/// The display width MariaDB prints for an integer type — one narrower for
/// UNSIGNED (no sign character): `int(11)` / `int(10) unsigned`.
fn mysql_int_display_width(base: &str, unsigned: bool) -> u8 {
    match (base, unsigned) {
        ("tinyint", false) => 4,
        ("tinyint", true) => 3,
        ("smallint", false) => 6,
        ("smallint", true) => 5,
        ("mediumint", false) => 9,
        ("mediumint", true) => 8,
        ("int", false) => 11,
        ("int", true) => 10,
        _ => 20, // bigint (signed and unsigned both 20)
    }
}

/// v7.39 (round 358, M16) — MySQL's spelling of a column type: lower
/// case, with the display width MariaDB prints (`int(11)`, `bigint(20)`),
/// the narrow name (`tinyint(4)`, `mediumint(9)`) and the ` unsigned`
/// suffix (round 389, epic P4a).
pub(crate) fn render_mysql_type(col: &ColumnSchema) -> String {
    if let Some(base) = mysql_int_base_name(col.ty, col.mysql_int_width) {
        let width = mysql_int_display_width(base, col.is_unsigned);
        let mut s = alloc::format!("{base}({width})");
        if col.is_unsigned {
            s.push_str(" unsigned");
        }
        return s;
    }
    match col.ty {
        DataType::Float => "double".into(),
        DataType::Real => "float".into(),
        DataType::Text => "text".into(),
        DataType::Varchar(n) => alloc::format!("varchar({n})"),
        DataType::Char(n) => alloc::format!("char({n})"),
        DataType::Bool => "tinyint(1)".into(),
        DataType::Numeric { precision, scale } => alloc::format!("decimal({precision},{scale})"),
        DataType::Date => "date".into(),
        DataType::Timestamp | DataType::Timestamptz => "datetime".into(),
        DataType::Time => "time".into(),
        DataType::Bytes => "blob".into(),
        DataType::Json | DataType::Jsonb => "json".into(),
        // Anything without a MySQL spelling keeps SPG's own, lower-cased.
        other => render_data_type(other).to_ascii_lowercase(),
    }
}

/// MariaDB prints `current_timestamp()` for the clock default and an
/// unquoted number for a numeric one; anything else goes through as the
/// stored text.
fn mysql_default_text(d: &str) -> String {
    let t = d.trim();
    if t.eq_ignore_ascii_case("current_timestamp") || t.eq_ignore_ascii_case("now()") {
        return "current_timestamp()".into();
    }
    alloc::string::String::from(t)
}

fn render_data_type(ty: DataType) -> String {
    match ty {
        DataType::SmallInt => "SMALLINT".into(),
        DataType::Int => "INT".into(),
        DataType::BigInt => "BIGINT".into(),
        DataType::Float => "FLOAT".into(),
        DataType::Real => "REAL".into(),
        DataType::Text => "TEXT".into(),
        DataType::Name => "NAME".into(),
        DataType::Xid => "XID".into(),
        DataType::Xid8 => "XID8".into(),
        DataType::Oid => "OID".into(),
        DataType::OidArray => "OID[]".into(),
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
        DataType::BoolArray2D => "BOOL[][]".into(),
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
        DataType::PgLsn => "PG_LSN".into(),
        DataType::Bit(_) => "BIT".into(),
        DataType::BitVarying(_) => "VARBIT".into(),
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

/// Case-insensitive MySQL LIKE for variable-name filtering: `%` any
/// sequence, `_` exactly one character, everything else literal.
fn mysql_like_ci(text: &str, pattern: &str) -> bool {
    fn rec(t: &[u8], p: &[u8]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some(b'%') => (0..=t.len()).any(|k| rec(&t[k..], &p[1..])),
            Some(b'_') => !t.is_empty() && rec(&t[1..], &p[1..]),
            Some(&c) => !t.is_empty() && t[0].eq_ignore_ascii_case(&c) && rec(&t[1..], &p[1..]),
        }
    }
    rec(text.as_bytes(), pattern.as_bytes())
}
