//! v7.38 (read01, T28) — ALTER COLUMN ... DROP IDENTITY [IF EXISTS]:
//! de-generate an identity column into a plain column; error on a non-identity
//! column unless IF EXISTS. Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| r.values.iter().map(|v| format!("{v:?}")).collect())
            .collect(),
        _ => vec![],
    }
}

#[test]
fn alter_column_drop_identity() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t28(id int GENERATED ALWAYS AS IDENTITY, x int)")
        .unwrap();
    e.execute("ALTER TABLE t28 ALTER COLUMN id DROP IDENTITY")
        .unwrap();
    // After dropping identity the column is plain — an explicit id inserts fine.
    e.execute("INSERT INTO t28(id, x) VALUES (5, 10)").unwrap();
    assert_eq!(rows(&mut e, "SELECT id, x FROM t28"), vec![vec!["Int(5)", "Int(10)"]]);

    // DROP IDENTITY on a non-identity column errors, but IF EXISTS is a no-op.
    e.execute("CREATE TABLE t28b(a int)").unwrap();
    assert!(e.execute("ALTER TABLE t28b ALTER COLUMN a DROP IDENTITY").is_err());
    assert!(e
        .execute("ALTER TABLE t28b ALTER COLUMN a DROP IDENTITY IF EXISTS")
        .is_ok());
}
