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

// read01 — pg_get_constraintdef also reconstructs CHECK and (PG 18)
// NOT NULL constraints, by name or by pg_constraint OID. vs live PG 18.4.
#[test]
fn constraintdef_check_and_not_null() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE cc (id INT NOT NULL, a INT CHECK (a > 0), b TEXT)")
        .unwrap();
    // CHECK — PG wraps the predicate: `CHECK ((a > 0))`.
    assert_eq!(
        text(&first(&mut e, "SELECT pg_get_constraintdef('cc_check')")),
        "CHECK ((a > 0))"
    );
    // NOT NULL — one per NOT NULL column.
    assert_eq!(
        text(&first(&mut e, "SELECT pg_get_constraintdef('cc_id_not_null')")),
        "NOT NULL id"
    );
    // Same via the OID form (pg_constraint.oid), as pg_dump uses.
    let via_oid = first(
        &mut e,
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
         WHERE conrelid = (SELECT oid FROM pg_class WHERE relname = 'cc') \
           AND contype = 'c'",
    );
    assert_eq!(text(&via_oid), "CHECK ((a > 0))");
}
