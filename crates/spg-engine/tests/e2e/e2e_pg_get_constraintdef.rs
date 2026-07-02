//! v7.37.17 (17.6 siblings) — pg_get_constraintdef upgraded from
//! NULL stub to real PK/UNIQUE/FK reconstruction.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn constraintdef_primary_key() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE cp (id INT PRIMARY KEY, name TEXT)")
        .unwrap();
    assert_eq!(
        text(&first(&mut e, "SELECT pg_get_constraintdef('cp_pkey')")),
        "PRIMARY KEY (id)"
    );
}

#[test]
fn constraintdef_foreign_key() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE parent (id INT PRIMARY KEY)").unwrap();
    e.execute(
        "CREATE TABLE child (pid INT, \
         CONSTRAINT child_pid_fk FOREIGN KEY (pid) \
         REFERENCES parent(id) ON DELETE CASCADE)",
    )
    .unwrap();
    let def = text(&first(
        &mut e,
        "SELECT pg_get_constraintdef('child_pid_fk')",
    ));
    assert_eq!(
        def,
        "FOREIGN KEY (pid) REFERENCES parent(id) ON DELETE CASCADE"
    );
}

#[test]
fn constraintdef_unknown_is_null() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT pg_get_constraintdef('no_such')"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT pg_get_constraintdef(NULL::text)"),
        spg_storage::Value::Null
    ));
}
