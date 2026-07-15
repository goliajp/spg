//! v7.39 (read01 round 88) — INSERT-time error messages aligned to PG.
//!
//! A differential sweep of the DDL / constraint error surface found two INSERT
//! errors that clients match on but SPG worded its own way:
//!
//!   * value/column count mismatch came out as SPG's generic
//!     "row arity mismatch: expected N columns, got M" instead of PG's two
//!     42601 messages ("INSERT has more expressions than target columns" /
//!     "INSERT has more target columns than expressions");
//!   * a missing INSERT target column dropped the relation name — SPG said
//!     `column "x" does not exist` where PG says
//!     `column "x" of relation "t" does not exist` (42703).

use spg_engine::Engine;

fn err(e: &mut Engine, sql: &str) -> String {
    e.execute(sql).unwrap_err().to_string()
}

#[test]
fn a_more_values_than_target_columns() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a int, b int, c int)").unwrap();
    // Explicit column list, more values.
    assert!(
        err(&mut e, "INSERT INTO t(a,b) VALUES (1,2,3)")
            .contains("INSERT has more expressions than target columns"),
    );
    // No column list, more values than the table has columns.
    assert!(
        err(&mut e, "INSERT INTO t VALUES (1,2,3,4)")
            .contains("INSERT has more expressions than target columns"),
    );
}

#[test]
fn b_fewer_values_than_target_columns() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a int, b int, c int)").unwrap();
    // Explicit column list, fewer values.
    assert!(
        err(&mut e, "INSERT INTO t(a,b,c) VALUES (1,2)")
            .contains("INSERT has more target columns than expressions"),
    );
}

#[test]
fn c_no_column_list_fewer_values_is_ok() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a int, b int, c int)").unwrap();
    // Without a column list, a short row fills the trailing columns with NULL —
    // PG allows this, only MORE values is an error.
    e.execute("INSERT INTO t VALUES (1,2)").unwrap();
    match e.execute("SELECT c FROM t").unwrap() {
        spg_engine::QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0].values[0], spg_storage::Value::Null);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn d_missing_target_column_names_the_relation() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a int, b int)").unwrap();
    assert!(
        err(&mut e, "INSERT INTO t(a, nosuchcol) VALUES (1, 2)")
            .contains("column \"nosuchcol\" of relation \"t\" does not exist"),
    );
}
