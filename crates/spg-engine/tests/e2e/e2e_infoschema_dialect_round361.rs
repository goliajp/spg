//! read01 round 361 (MySQL differential, M18) — information_schema.data_type.
//!
//! `information_schema.columns.data_type` reported PG's type names on
//! every session. A MySQL reflection tool — SQLAlchemy's mysql dialect,
//! JDBC getColumns — reads this to choose the column's Python / Java
//! type, so `timestamp without time zone` where it expected `datetime`,
//! or `numeric` where it expected `decimal`, sent it down the wrong
//! branch or failed to map the column at all.
//!
//! Measured, both oracles, same nine-column table:
//!   PG 18.4   : integer / character varying / text / numeric /
//!               timestamp without time zone / date / bigint /
//!               double precision / bytea
//!   MariaDB 11: int / varchar / text / decimal / datetime / date /
//!               bigint / double / blob
//!
//! PG sessions are unchanged; only the MySQL dialect reports MySQL names.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn engine(mysql: bool) -> Engine {
    let mut e = Engine::new();
    if mysql {
        e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    }
    e.execute(
        "CREATE TABLE ty (a INT, b VARCHAR(10), c TEXT, d DECIMAL(10,2), \
                          e DATETIME, f DATE, h BIGINT, i DOUBLE, j BLOB)",
    )
    .unwrap();
    e
}

fn data_types(e: &mut Engine) -> Vec<String> {
    match e
        .execute(
            "SELECT data_type FROM information_schema.columns \
               WHERE table_name = 'ty' ORDER BY ordinal_position",
        )
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match r.values.first() {
                Some(Value::Text(t)) => t.to_string(),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_mysql_session_reports_mysql_names() {
    let mut e = engine(true);
    assert_eq!(
        data_types(&mut e),
        [
            "int", "varchar", "text", "decimal", "datetime", "date", "bigint", "double", "blob",
        ],
    );
}

#[test]
fn a_pg_session_still_reports_pg_names() {
    let mut e = engine(false);
    assert_eq!(
        data_types(&mut e),
        [
            "integer",
            "character varying",
            "text",
            "numeric",
            "timestamp without time zone",
            "date",
            "bigint",
            "double precision",
            "bytea",
        ],
    );
}

/// The distinction a reflection tool depends on: the four names that
/// differ between the dialects, named so a regression says which.
#[test]
fn the_four_that_differ() {
    let pg = data_types(&mut engine(false));
    let my = data_types(&mut engine(true));
    // int vs integer
    assert_eq!((pg[0].as_str(), my[0].as_str()), ("integer", "int"));
    // numeric vs decimal
    assert_eq!((pg[3].as_str(), my[3].as_str()), ("numeric", "decimal"));
    // timestamp without time zone vs datetime
    assert_eq!(
        (pg[4].as_str(), my[4].as_str()),
        ("timestamp without time zone", "datetime")
    );
    // bytea vs blob
    assert_eq!((pg[8].as_str(), my[8].as_str()), ("bytea", "blob"));
}
