//! v7.39 (read01 commands/, round 49) — the remaining ALTER TABLE /
//! ALTER TYPE / ALTER SEQUENCE subcommands. Two silent-correctness bugs
//! (SET NOT NULL counted tombstoned rows; ALTER TYPE RENAME VALUE was a
//! no-op), one broken composition (unnest over enum_range returned zero
//! rows), and two missing forms (ALTER SEQUENCE RENAME TO, ALTER TABLE
//! REPLICA IDENTITY). Byte-locked vs PG18.

use spg_engine::{Engine, QueryResult};

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).unwrap_err())
}

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn set_not_null_scans_visible_rows_only() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE s1(a int NOT NULL, b text)");
    ok(&mut e, "ALTER TABLE s1 ALTER COLUMN a DROP NOT NULL");
    ok(&mut e, "INSERT INTO s1(a,b) VALUES (NULL,'x')");
    // A real NULL blocks it, with PG's wording (23502 at the wire).
    assert!(
        err(&mut e, "ALTER TABLE s1 ALTER COLUMN a SET NOT NULL")
            .contains("column \"a\" of relation \"s1\" contains null values")
    );
    // After DELETE the table is empty to PG. Under in-place MVCC the row is
    // only tombstoned — counting physical rows used to keep failing here.
    ok(&mut e, "DELETE FROM s1");
    ok(&mut e, "ALTER TABLE s1 ALTER COLUMN a SET NOT NULL");
}

#[test]
fn alter_type_rename_value() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TYPE mood AS ENUM ('sad','ok')");
    ok(&mut e, "ALTER TYPE mood ADD VALUE 'happy'");
    // Used to be swallowed as a no-op: accepted, label unchanged.
    ok(&mut e, "ALTER TYPE mood RENAME VALUE 'sad' TO 'unhappy'");
    // Renaming in place keeps the sort position (PG leaves enumsortorder).
    assert_eq!(
        col(&mut e, "SELECT enum_range(NULL::mood)"),
        vec!["{unhappy,ok,happy}"]
    );
    assert!(
        err(&mut e, "ALTER TYPE mood ADD VALUE 'ok'").contains("enum label \"ok\" already exists")
    );
    ok(&mut e, "ALTER TYPE mood ADD VALUE IF NOT EXISTS 'ok'");
}

#[test]
fn unnest_over_enum_range() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TYPE mood AS ENUM ('sad','ok','happy')");
    // The enum introspection family resolves its labels from the argument's
    // static type against the catalog — the FROM-clause unnest context had
    // no catalog, so this expanded to zero rows.
    assert_eq!(
        col(&mut e, "SELECT unnest(enum_range(NULL::mood))"),
        vec!["sad", "ok", "happy"]
    );
}

#[test]
fn alter_sequence_rename_and_replica_identity() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE SEQUENCE sq1 START 5 INCREMENT 2");
    assert_eq!(col(&mut e, "SELECT nextval('sq1')"), vec!["5"]);
    ok(&mut e, "ALTER SEQUENCE sq1 RENAME TO sq2");
    assert_eq!(col(&mut e, "SELECT nextval('sq2')"), vec!["7"]);
    // REPLICA IDENTITY used to be a parse error; SPG replicates SQL text, so
    // there is no old-tuple image to configure — accept and no-op.
    ok(&mut e, "CREATE TABLE s2(a int)");
    ok(&mut e, "ALTER TABLE s2 REPLICA IDENTITY FULL");
}
