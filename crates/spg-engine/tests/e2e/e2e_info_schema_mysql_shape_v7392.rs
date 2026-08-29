//! v7.39.2 — the catalog views answer in MySQL's shape on a MySQL
//! session, and under the session's own database name.
//!
//! `SELECT … FROM information_schema.tables WHERE table_schema =
//! DATABASE()` is THE reflection query on MySQL — Django's
//! introspection, Rails' schema dumper, every JDBC browser. SPG
//! answered PostgreSQL's `public` in that column, so the query matched
//! nothing and reported a database with no tables in it.
//!
//! The shapes were PostgreSQL's too, with two MySQL columns bolted on
//! the end. Measured on MySQL 9.7.2: `COLUMNS` has 22 columns in its own
//! order, six of which were simply absent, and `SCHEMATA` has six where
//! SPG served PostgreSQL's seven plus one.
//!
//! And the VALUES differ where the column names agree, so each dialect
//! needs its own answer rather than one shared one: an INT reports
//! `NUMERIC_PRECISION` 10 (decimal digits) on MySQL and 32 (bits) on
//! PostgreSQL; TEXT reports 65535 for both length columns where PG
//! reports NULL; an AUTO_INCREMENT column's `COLUMN_DEFAULT` is NULL
//! there with `EXTRA = auto_increment`, where SPG answered PG's
//! `nextval('t_id_seq'::regclass)` — a reflection copying that into
//! MySQL DDL writes a statement MySQL cannot parse.

use spg_engine::{Engine, QueryResult};

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.set_mysql_dialect(true);
    e
}

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match e.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect()
            })
            .collect(),
        Ok(other) => panic!("{sql}: not rows: {other:?}"),
        Err(err) => panic!("{sql}: {err}"),
    }
}

fn one(e: &mut Engine, sql: &str) -> String {
    rows(e, sql)
        .first()
        .and_then(|r| r.first())
        .cloned()
        .unwrap_or_else(|| "<none>".to_string())
}

const DDL: &str = "CREATE TABLE isk (id INT AUTO_INCREMENT PRIMARY KEY, \
     u VARCHAR(8) UNIQUE, m INT, s INT DEFAULT 7, t TEXT, bl BLOB, \
     dc DECIMAL(10,2), bg BIGINT, sm SMALLINT)";

#[test]
fn the_reflection_query_finds_the_tables() {
    let mut e = mysql();
    e.execute("USE appdb").expect("USE");
    e.execute(DDL).expect("ddl");
    // The query itself, in the spelling every MySQL tool writes it.
    assert_eq!(
        one(
            &mut e,
            "SELECT COUNT(*) FROM information_schema.tables \
             WHERE table_schema = DATABASE() AND table_name = 'isk'"
        ),
        "1"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT COUNT(*) FROM information_schema.columns \
             WHERE table_schema = DATABASE() AND table_name = 'isk'"
        ),
        "9"
    );
    // And it follows the session, because one database is served under
    // whatever name this session is using.
    e.execute("USE other").expect("USE");
    assert_eq!(
        one(
            &mut e,
            "SELECT table_schema FROM information_schema.tables WHERE table_name = 'isk'"
        ),
        "other"
    );
    // A session that never named one still finds its tables.
    let mut fresh = mysql();
    fresh.execute(DDL).expect("ddl");
    assert_eq!(
        one(
            &mut fresh,
            "SELECT table_schema FROM information_schema.tables WHERE table_name = 'isk'"
        ),
        "spg"
    );
}

#[test]
fn the_columns_view_has_mysqls_shape_and_values() {
    let mut e = mysql();
    e.execute(DDL).expect("ddl");
    // Measured on MySQL 9.7.2, in its own column order.
    let got = rows(
        &mut e,
        "SELECT column_name, data_type, character_maximum_length, \
         character_octet_length, numeric_precision, numeric_scale, \
         column_key, extra, column_default, privileges, column_comment, srs_id \
         FROM information_schema.columns WHERE table_name = 'isk' \
         ORDER BY ordinal_position",
    );
    let want: &[&[&str]] = &[
        &[
            "id",
            "int",
            "NULL",
            "NULL",
            "10",
            "0",
            "PRI",
            "auto_increment",
            "NULL",
            "select,insert,update,references",
            "",
            "NULL",
        ],
        &[
            "u",
            "varchar",
            "8",
            "32",
            "NULL",
            "NULL",
            "UNI",
            "",
            "NULL",
            "select,insert,update,references",
            "",
            "NULL",
        ],
        &[
            "m",
            "int",
            "NULL",
            "NULL",
            "10",
            "0",
            "",
            "",
            "NULL",
            "select,insert,update,references",
            "",
            "NULL",
        ],
        &[
            "s",
            "int",
            "NULL",
            "NULL",
            "10",
            "0",
            "",
            "",
            "7",
            "select,insert,update,references",
            "",
            "NULL",
        ],
        &[
            "t",
            "text",
            "65535",
            "65535",
            "NULL",
            "NULL",
            "",
            "",
            "NULL",
            "select,insert,update,references",
            "",
            "NULL",
        ],
        &[
            "bl",
            "blob",
            "65535",
            "65535",
            "NULL",
            "NULL",
            "",
            "",
            "NULL",
            "select,insert,update,references",
            "",
            "NULL",
        ],
        &[
            "dc",
            "decimal",
            "NULL",
            "NULL",
            "10",
            "2",
            "",
            "",
            "NULL",
            "select,insert,update,references",
            "",
            "NULL",
        ],
        &[
            "bg",
            "bigint",
            "NULL",
            "NULL",
            "19",
            "0",
            "",
            "",
            "NULL",
            "select,insert,update,references",
            "",
            "NULL",
        ],
        &[
            "sm",
            "smallint",
            "NULL",
            "NULL",
            "5",
            "0",
            "",
            "",
            "NULL",
            "select,insert,update,references",
            "",
            "NULL",
        ],
    ];
    assert_eq!(got.len(), want.len(), "{got:#?}");
    for (g, w) in got.iter().zip(want) {
        assert_eq!(g.as_slice(), *w);
    }
    // The whole view, by position: a client reading `SELECT *` gets
    // MySQL's 22 columns and not PostgreSQL's with two appended.
    let QueryResult::Rows { columns, .. } = e
        .execute("SELECT * FROM information_schema.columns WHERE table_name = 'isk'")
        .expect("star")
    else {
        panic!("not rows")
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "table_catalog",
            "table_schema",
            "table_name",
            "column_name",
            "ordinal_position",
            "column_default",
            "is_nullable",
            "data_type",
            "character_maximum_length",
            "character_octet_length",
            "numeric_precision",
            "numeric_scale",
            "datetime_precision",
            "character_set_name",
            "collation_name",
            "column_type",
            "column_key",
            "extra",
            "privileges",
            "column_comment",
            "generation_expression",
            "srs_id",
        ]
    );
}

#[test]
fn the_tables_and_schemata_views_have_mysqls_shape() {
    let mut e = mysql();
    e.execute("CREATE TABLE tt (a INT)").expect("ddl");
    let QueryResult::Rows { columns, .. } = e
        .execute("SELECT * FROM information_schema.tables WHERE table_name = 'tt'")
        .expect("star")
    else {
        panic!("not rows")
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "table_catalog",
            "table_schema",
            "table_name",
            "table_type",
            "engine",
            "version",
            "row_format",
            "table_rows",
            "avg_row_length",
            "data_length",
            "max_data_length",
            "index_length",
            "data_free",
            "auto_increment",
            "create_time",
            "update_time",
            "check_time",
            "table_collation",
            "checksum",
            "create_options",
            "table_comment",
        ]
    );
    // The two a MySQL reflection reads off a table, which previously
    // errored with "column does not exist".
    assert_eq!(
        rows(
            &mut e,
            "SELECT engine, row_format, table_collation, table_type \
             FROM information_schema.tables WHERE table_name = 'tt'"
        )[0],
        ["InnoDB", "Dynamic", "utf8mb4_0900_ai_ci", "BASE TABLE"]
    );

    let QueryResult::Rows { columns, .. } = e
        .execute("SELECT * FROM information_schema.schemata")
        .expect("star")
    else {
        panic!("not rows")
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "catalog_name",
            "schema_name",
            "default_character_set_name",
            "default_collation_name",
            "sql_path",
            "default_encryption",
        ]
    );
}

/// The control: a PostgreSQL session keeps PostgreSQL's shape and
/// PostgreSQL's values. The two dialects disagree about the same column
/// name on purpose — `numeric_precision` is 32 bits here and 10 digits
/// there — so a shared answer would have to be wrong for one of them.
#[test]
fn a_postgres_session_keeps_postgress_shape() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE isk (id INT, t TEXT)").expect("ddl");
    assert_eq!(
        rows(
            &mut e,
            "SELECT table_schema, numeric_precision, character_maximum_length \
             FROM information_schema.columns WHERE table_name = 'isk' \
             ORDER BY ordinal_position"
        ),
        [["public", "32", "NULL"], ["public", "NULL", "NULL"],]
    );
    // `column_type` is MySQL's and does not exist here.
    assert!(
        e.execute("SELECT column_type FROM information_schema.columns")
            .is_err()
    );
}
