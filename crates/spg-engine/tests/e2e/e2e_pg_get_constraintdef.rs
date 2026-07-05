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
        text(&first(&mut e, "SELECT pg_get_constraintdef('cc_a_check')")),
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

// read01 — UNIQUE constraints use PG's auto-name convention
// `{table}_{col…}_key`, consistent across pg_constraint,
// pg_get_constraintdef, and `ON CONFLICT ON CONSTRAINT`. vs live PG 18.4.
#[test]
fn unique_constraint_pg_naming() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE nm (a INT, b INT, c INT, UNIQUE(a), UNIQUE(b, c))")
        .unwrap();
    let names = {
        let r = e
            .execute(
                "SELECT conname FROM pg_constraint \
                 WHERE conrelid = (SELECT oid FROM pg_class WHERE relname = 'nm') \
                   AND contype = 'u' ORDER BY conname",
            )
            .unwrap();
        let QueryResult::Rows { rows, .. } = r else { panic!() };
        rows.iter()
            .filter_map(|row| match &row.values[0] {
                spg_storage::Value::Text(s) => Some(s.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(names, ["nm_a_key", "nm_b_c_key"]);
    assert_eq!(
        text(&first(&mut e, "SELECT pg_get_constraintdef('nm_b_c_key')")),
        "UNIQUE (b, c)"
    );
    // The name resolves for ON CONFLICT ON CONSTRAINT.
    e.execute("INSERT INTO nm VALUES (1, 2, 3)").unwrap();
    e.execute("INSERT INTO nm VALUES (1, 9, 9) ON CONFLICT ON CONSTRAINT nm_a_key DO NOTHING")
        .unwrap();
    let QueryResult::Rows { rows, .. } =
        e.execute("SELECT count(*) FROM nm").unwrap()
    else {
        panic!()
    };
    assert!(matches!(rows[0].values[0], spg_storage::Value::BigInt(1)));
}

// read01 — CHECK auto-names follow PG: single-column check
// `{t}_{col}_check`, multi-column `{t}_check` (+ collision suffix), with
// string literals skipped. vs live PG 18.4.
#[test]
fn check_constraint_pg_naming() {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE ck (a INT CHECK (a > 0), b INT, c INT, \
         CHECK (b > c), CHECK (a + b > 0))",
    )
    .unwrap();
    let names = {
        let r = e
            .execute(
                "SELECT conname FROM pg_constraint \
                 WHERE conrelid = (SELECT oid FROM pg_class WHERE relname = 'ck') \
                   AND contype = 'c' ORDER BY conname",
            )
            .unwrap();
        let QueryResult::Rows { rows, .. } = r else { panic!() };
        rows.iter()
            .filter_map(|row| match &row.values[0] {
                spg_storage::Value::Text(s) => Some(s.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    // a>0 → ck_a_check; b>c and a+b>0 are multi-column → ck_check / ck_check1.
    assert_eq!(names, ["ck_a_check", "ck_check", "ck_check1"]);
    // A column name inside a string literal is not matched.
    e.execute("CREATE TABLE q (name TEXT CHECK (name <> 'x'))")
        .unwrap();
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT conname FROM pg_constraint \
             WHERE conrelid = (SELECT oid FROM pg_class WHERE relname = 'q') AND contype = 'c'",
        )),
        "q_name_check"
    );
}
