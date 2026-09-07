//! The one MySQL system-variable inventory.
//!
//! `SHOW VARIABLES` and `@@name` are two surfaces on the same question,
//! and this file exists because they were two separate tables. Every
//! release since v7.39 has found another name present in one and absent
//! from the other — `collation_database`, `character_sets_dir`,
//! `authentication_policy`, `time_zone` — and fixed that one name in
//! both places, which leaves the next one to be found by a customer.
//!
//! The constant part of the inventory now has a single definition and
//! both surfaces render it, so a name cannot be added to one alone. The
//! entries that are genuinely computed from the session or the engine
//! (`time_zone`, `lower_case_table_names`, `transaction_isolation`,
//! `warning_count`) still live at their two call sites, and the e2e pin
//! `e2e_mysql_variable_surfaces` walks every row of `SHOW VARIABLES`
//! asking `@@name` for it, so drift there is caught by enumeration
//! rather than by the next report.

/// MySQL renders a value differently per SURFACE, not per variable:
/// measured on 9.7.2, `SHOW VARIABLES LIKE 'autocommit'` answers `ON`
/// where `SELECT @@autocommit` answers `1`, while a number or a string
/// is spelled identically on both.
#[derive(Clone, Copy)]
pub(crate) enum VarValue {
    Bool(bool),
    Text(&'static str),
}

impl VarValue {
    pub(crate) const fn show(self) -> &'static str {
        match self {
            Self::Bool(true) => "ON",
            Self::Bool(false) => "OFF",
            Self::Text(t) => t,
        }
    }

    pub(crate) const fn at_at(self) -> &'static str {
        match self {
            Self::Bool(true) => "1",
            Self::Bool(false) => "0",
            Self::Text(t) => t,
        }
    }
}

/// SPG's own licence, read from the manifest so it cannot drift from
/// what the crates are actually published under. MySQL answers `GPL`
/// here; repeating that would be a false claim about this software.
const SPG_LICENSE: &str = env!("CARGO_PKG_LICENSE");

/// The names whose value is the same for every session.
///
/// Sorted, because `SHOW VARIABLES` on MySQL 9.7.2 is sorted by name
/// (measured across all 655 of its rows).
///
/// Every name here exists on MySQL 9.7.2 — checked row by row against
/// the oracle's own `SHOW VARIABLES` — and every value is a true
/// statement about SPG rather than a copy of MySQL's. Where the two
/// differ the difference is deliberate and noted.
pub(crate) const CONSTANT: &[(&str, VarValue)] = &[
    // v7.40.11 — measured on SPG: three rows inserted into a table with
    // an `INT AUTO_INCREMENT` key came back 1, 2, 3. Connector/J reads
    // this one FIRST of the nineteen it asks for.
    ("auto_increment_increment", VarValue::Text("1")),
    ("auto_increment_offset", VarValue::Text("1")),
    // v7.40.11 — `default_authentication_plugin` was REMOVED and this
    // replaced it. Measured on stock `mysql:9.7.2`: `*,,` — the
    // compiled-in default, which is `caching_sha2_password`, and is
    // what SPG verifies.
    (
        "authentication_policy",
        VarValue::Text(crate::MYSQL_AUTHENTICATION_POLICY),
    ),
    // Autocommit is on until the session says otherwise; the wire keeps
    // the same answer in its status flags.
    ("autocommit", VarValue::Bool(true)),
    // v7.39 — these were missing from one surface or the other, so a
    // session asking for either got `Unknown system variable` where
    // MySQL 9.7.2 answers. ORMs read them to reflect a schema; a hard
    // error there is not a gap the caller can work around.
    //
    // The value is a true statement about this database: measured, a
    // MySQL-dialect session compares `'A' = 'a'` as equal, which is
    // what `utf8mb4_0900_ai_ci` means. The connection pair follows
    // `SET NAMES` — the session lookup at both call sites runs before
    // this table — while the database-scoped names do not, which is how
    // MySQL scopes them.
    ("character_set_client", VarValue::Text("utf8mb4")),
    ("character_set_connection", VarValue::Text("utf8mb4")),
    ("character_set_database", VarValue::Text("utf8mb4")),
    // `binary` on MySQL, which means names are used as bytes rather
    // than transcoded. That is true here too.
    ("character_set_filesystem", VarValue::Text("binary")),
    ("character_set_results", VarValue::Text("utf8mb4")),
    ("character_set_server", VarValue::Text("utf8mb4")),
    // What the server stores IDENTIFIERS in. MySQL 9.7.2 says `utf8mb3`
    // because it cannot hold a four-byte one: measured, it stores
    // `` `z4b😀` `` as `z4b?`. SPG keeps `z4b😀`, so the true answer
    // here is `utf8mb4` and it differs from MySQL's on purpose —
    // reporting `utf8mb3` to match would be a claim about SPG that its
    // own catalog contradicts.
    ("character_set_system", VarValue::Text("utf8mb4")),
    // A directory of charset definition files. SPG has no such
    // directory — its charsets are compiled in — so this is a claim
    // about the SHAPE of the answer, not about this filesystem, the
    // same kind of claim as reporting `ENGINE=InnoDB` for a table SPG
    // stores its own way. The path is the one MySQL 9.7.2 reports,
    // because that is the version SPG answers as.
    (
        "character_sets_dir",
        VarValue::Text(crate::MYSQL_CHARACTER_SETS_DIR),
    ),
    (
        "collation_connection",
        VarValue::Text(crate::collate::MYSQL_DEFAULT_CONNECTION_COLLATION),
    ),
    (
        "collation_database",
        VarValue::Text(crate::collate::MYSQL_DEFAULT_CONNECTION_COLLATION),
    ),
    (
        "collation_server",
        VarValue::Text(crate::collate::MYSQL_DEFAULT_CONNECTION_COLLATION),
    ),
    ("foreign_key_checks", VarValue::Bool(true)),
    // v7.40.11 — the statement the server runs for each connecting
    // client. SPG runs none, and MySQL's own default is empty too.
    ("init_connect", VarValue::Text("")),
    ("innodb_stats_on_metadata", VarValue::Bool(false)),
    // v7.40.11 — seconds of idle before the server closes the
    // connection. Measured: with `SPG_IDLE_TIMEOUT_SEC=2` set, a
    // mysql-wire connection was still answering after six seconds
    // idle — that limit is applied by the native protocol handler and
    // by neither wire host. So SPG never closes an idle mysql-wire
    // session, and `0` says exactly that. MySQL's 28800 would be a
    // promise SPG does not keep in either direction.
    ("interactive_timeout", VarValue::Text("0")),
    ("license", VarValue::Text(SPG_LICENSE)),
    // Measured on MySQL 9.7.2: 67108864 on both surfaces.
    ("max_allowed_packet", VarValue::Text("67108864")),
    // Neither wire host sets a socket read or write timeout at all;
    // see `interactive_timeout` above.
    ("net_read_timeout", VarValue::Text("0")),
    ("net_write_timeout", VarValue::Text("0")),
    // v7.40.11 — MySQL 9.7.2 ships it ON, MariaDB 12.3.3 OFF (both
    // measured). SPG has no performance schema, so `OFF` is the true
    // answer and a client that reads it will not go looking for tables
    // that are not there.
    ("performance_schema", VarValue::Bool(false)),
    ("sql_mode", VarValue::Text(crate::MYSQL_DEFAULT_SQL_MODE)),
    ("sql_notes", VarValue::Bool(true)),
    ("sql_quote_show_create", VarValue::Bool(true)),
    ("system_time_zone", VarValue::Text("UTC")),
    ("unique_checks", VarValue::Bool(true)),
    ("version", VarValue::Text(crate::MYSQL_SERVER_VERSION)),
    (
        "version_comment",
        VarValue::Text(crate::MYSQL_VERSION_COMMENT),
    ),
    ("version_compile_os", VarValue::Text(crate::MYSQL_COMPILE_OS)),
    ("wait_timeout", VarValue::Text("0")),
];

/// The constant inventory by name, case-insensitively — MySQL system
/// variable names compare caselessly.
pub(crate) fn constant(name: &str) -> Option<VarValue> {
    CONSTANT
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|&(_, v)| v)
}

/// Does the constant inventory carry this name? Used by the two
/// surfaces to decide whether a name they compute is already covered.
pub(crate) fn is_constant(name: &str) -> bool {
    constant(name).is_some()
}
