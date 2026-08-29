//! read01 round 362 (MySQL differential, M18 tail) — the column_type column.
//!
//! MySQL's `information_schema.columns` carries a `column_type` column PG
//! has no equivalent of: the full declared type with its length and its
//! unsigned-ness (`varchar(10)`, `decimal(10,2)`, `int unsigned`), which
//! SQLAlchemy's mysql reflection reads.
//! Naming it on a MySQL session errored, `column "column_type" does not
//! exist`, so a reflection pass could not complete.
//!
//! It is appended only in the MySQL dialect, so the view keeps PG's shape
//! on a PG session — where naming `column_type` still errors, as it does
//! in PG.
//!
//! v7.39.2 — RE-CALIBRATED against MySQL 9.7.2, the engine SPG
//! advertises itself as. These were a MariaDB 11 run, and MySQL dropped
//! the integer display width in 8.0.19.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e.execute(
        "CREATE TABLE ty (a INT, b VARCHAR(10), c TEXT, d DECIMAL(10,2), \
                          e DATETIME, f DATE, h BIGINT, i DOUBLE, j BLOB)",
    )
    .unwrap();
    e
}

fn col_types(e: &mut Engine) -> Vec<(String, String)> {
    match e
        .execute(
            "SELECT data_type, column_type FROM information_schema.columns \
               WHERE table_name = 'ty' ORDER BY ordinal_position",
        )
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match (&r.values[0], &r.values[1]) {
                (Value::Text(a), Value::Text(b)) => (a.to_string(), b.to_string()),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn column_type_carries_the_declared_type() {
    let mut e = mysql();
    assert_eq!(
        col_types(&mut e),
        vec![
            ("int".into(), "int".into()),
            ("varchar".into(), "varchar(10)".into()),
            ("text".into(), "text".into()),
            ("decimal".into(), "decimal(10,2)".into()),
            ("datetime".into(), "datetime".into()),
            ("date".into(), "date".into()),
            ("bigint".into(), "bigint".into()),
            ("double".into(), "double".into()),
            ("blob".into(), "blob".into()),
        ],
    );
}

/// A PG session has no such column, exactly as PG has none.
#[test]
fn a_pg_session_has_no_column_type() {
    let mut p = Engine::new();
    p.execute("CREATE TABLE ty (a INT)").unwrap();
    assert!(
        p.execute("SELECT column_type FROM information_schema.columns WHERE table_name = 'ty'")
            .is_err(),
        "PG's information_schema.columns has no column_type"
    );
    // …and its own columns still resolve.
    match p
        .execute("SELECT data_type FROM information_schema.columns WHERE table_name = 'ty'")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0].values[0], Value::text("integer"));
        }
        other => panic!("{other:?}"),
    }
}

/// The two columns agree — column_type is data_type plus the width.
#[test]
fn the_two_columns_are_consistent() {
    let mut e = mysql();
    for (dt, ct) in col_types(&mut e) {
        assert!(
            ct == dt || ct.starts_with(&format!("{dt}(")),
            "column_type {ct:?} should extend data_type {dt:?}"
        );
    }
}
