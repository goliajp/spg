//! v7.37.20 (20.13) — PL/pgSQL EXECUTE dynamic SQL.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<Value<'static>>> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("Rows");
    };
    rows.into_iter().map(|r| r.values).collect()
}

#[test]
fn execute_string_literal_creates_table() {
    let mut e = Engine::new();
    ddl(
        &mut e,
        "DO $$ BEGIN EXECUTE 'CREATE TABLE dyn_t (id INT)'; END $$;",
    );
    ddl(&mut e, "INSERT INTO dyn_t VALUES (1)");
    let rs = rows(&mut e, "SELECT id FROM dyn_t");
    assert_eq!(rs.len(), 1);
    assert_eq!(rs[0][0], Value::Int(1));
}

#[test]
fn execute_expression_computes_sql_at_runtime() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT, name TEXT)");
    ddl(
        &mut e,
        "DO $$ DECLARE tab TEXT := 't'; \
         BEGIN EXECUTE 'INSERT INTO ' || tab || ' VALUES (1, ''alice'')'; END $$;",
    );
    let rs = rows(&mut e, "SELECT id, name FROM t");
    assert_eq!(rs.len(), 1);
    assert!(matches!(&rs[0][1], Value::Text(s) if s == "alice"));
}

#[test]
fn execute_bad_sql_raises_parse_error() {
    let mut e = Engine::new();
    let err = e.execute("DO $$ BEGIN EXECUTE 'THIS IS NOT SQL'; END $$;");
    assert!(err.is_err(), "bad EXECUTE SQL should error");
}
