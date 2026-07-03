//! UPDATE SET (cols) = (row/subquery) + SET col = DEFAULT.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.iter().map(|row| row.values.to_vec()).collect()
}

fn as_i64(v: &spg_storage::Value<'_>) -> i64 {
    match v {
        spg_storage::Value::Int(n) => i64::from(*n),
        spg_storage::Value::BigInt(n) => *n,
        other => panic!("expected integer, got {other:?}"),
    }
}

#[test]
fn multi_assign_row_values() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ma (id INT PRIMARY KEY, a INT, b INT)")
        .unwrap();
    e.execute("INSERT INTO ma VALUES (1, 0, 0)").unwrap();
    e.execute("UPDATE ma SET (a, b) = (7, 8) WHERE id = 1").unwrap();
    let got = rows(&mut e, "SELECT a, b FROM ma");
    assert_eq!(as_i64(&got[0][0]), 7);
    assert_eq!(as_i64(&got[0][1]), 8);
}

#[test]
fn multi_assign_subquery() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ms (id INT PRIMARY KEY, a INT, b INT)")
        .unwrap();
    e.execute("CREATE TABLE src (x INT, y INT)").unwrap();
    e.execute("INSERT INTO ms VALUES (1, 0, 0)").unwrap();
    e.execute("INSERT INTO src VALUES (11, 22)").unwrap();
    e.execute("UPDATE ms SET (a, b) = (SELECT x, y FROM src) WHERE id = 1")
        .unwrap();
    let got = rows(&mut e, "SELECT a, b FROM ms");
    assert_eq!(as_i64(&got[0][0]), 11);
    assert_eq!(as_i64(&got[0][1]), 22);
    // Arity mismatch errors.
    let err = e
        .execute("UPDATE ms SET (a, b) = (SELECT x FROM src)")
        .unwrap_err();
    assert!(format!("{err:?}").contains("arity"));
}

#[test]
fn set_default_restores_declared_default() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE sd (id INT PRIMARY KEY, tag TEXT DEFAULT 'fresh', n INT)")
        .unwrap();
    e.execute("INSERT INTO sd VALUES (1, 'dirty', 9)").unwrap();
    e.execute("UPDATE sd SET tag = DEFAULT WHERE id = 1").unwrap();
    let got = rows(&mut e, "SELECT tag, n FROM sd");
    assert!(matches!(&got[0][0], spg_storage::Value::Text(s) if s == "fresh"));
    assert_eq!(as_i64(&got[0][1]), 9);
    // A column with no declared default resets to NULL.
    e.execute("UPDATE sd SET n = DEFAULT WHERE id = 1").unwrap();
    let got = rows(&mut e, "SELECT n FROM sd");
    assert!(matches!(&got[0][0], spg_storage::Value::Null));
}
