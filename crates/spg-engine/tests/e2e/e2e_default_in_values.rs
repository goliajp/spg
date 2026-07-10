//! v7.38 (read01) — `INSERT INTO t VALUES (…, DEFAULT, …)`: a `DEFAULT`
//! keyword in a VALUES tuple uses the target column's declared default
//! (or next serial/identity value, or the generated-column computation, or
//! NULL for a column with no default). Every value / error is from live
//! PG18.4.

use spg_engine::{Engine, QueryResult};

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[0] {
                spg_storage::Value::Text(s) => s.to_string(),
                other => format!("{other:?}"),
            })
            .collect(),
        other => panic!("{sql}: expected Rows, got {other:?}"),
    }
}

#[test]
fn default_uses_declared_column_default() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(a int, b int DEFAULT 42)").unwrap();
    e.execute("INSERT INTO t VALUES (1, DEFAULT)").unwrap();
    e.execute("INSERT INTO t (a, b) VALUES (2, DEFAULT)").unwrap();
    // A DEFAULT in a permuted column list resolves the right column.
    e.execute("INSERT INTO t (b, a) VALUES (DEFAULT, 3)").unwrap();
    // Mixed DEFAULT / explicit across a multi-row INSERT.
    e.execute("INSERT INTO t VALUES (10, DEFAULT), (11, 5), (12, DEFAULT)").unwrap();
    assert_eq!(
        col(&mut e, "SELECT (a||':'||b)::text FROM t ORDER BY a"),
        vec!["1:42", "2:42", "3:42", "10:42", "11:5", "12:42"]
    );
}

#[test]
fn default_on_serial_takes_next_value() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(id serial, v text)").unwrap();
    e.execute("INSERT INTO t (id, v) VALUES (DEFAULT, 'x')").unwrap();
    e.execute("INSERT INTO t VALUES (DEFAULT, 'y')").unwrap();
    assert_eq!(col(&mut e, "SELECT (id||':'||v)::text FROM t ORDER BY id"), vec!["1:x", "2:y"]);
}

#[test]
fn default_on_generated_column_computes_it() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(a int, b int GENERATED ALWAYS AS (a*10) STORED)").unwrap();
    e.execute("INSERT INTO t VALUES (5, DEFAULT)").unwrap();
    e.execute("INSERT INTO t (a, b) VALUES (6, DEFAULT)").unwrap();
    assert_eq!(col(&mut e, "SELECT (a||':'||b)::text FROM t ORDER BY a"), vec!["5:50", "6:60"]);
    // An EXPLICIT value for a generated column is still rejected.
    assert!(e.execute("INSERT INTO t VALUES (7, 99)").is_err());
}

#[test]
fn default_is_null_for_a_column_without_a_default() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(a int, b int)").unwrap();
    e.execute("INSERT INTO t VALUES (1, DEFAULT)").unwrap();
    assert_eq!(col(&mut e, "SELECT COALESCE(b::text,'NULL') FROM t"), vec!["NULL"]);
    // DEFAULT → NULL against a NOT NULL column with no default is a violation.
    e.execute("CREATE TABLE nn(a int, b int NOT NULL)").unwrap();
    assert!(e.execute("INSERT INTO nn VALUES (1, DEFAULT)").is_err());
}
