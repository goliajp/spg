//! v7.39 (read01 round 46) — PG NOTICE emission. Every `IF EXISTS` /
//! `IF NOT EXISTS` clause that makes a DDL statement skip its work now
//! raises PG's "…, skipping" notice. The engine buffers them per
//! statement (`Engine::take_notices`); pgwire turns each into a
//! NoticeResponse. Wording byte-locked vs PG18.

use spg_engine::Engine;

fn notices_of(e: &mut Engine, sql: &str) -> Vec<String> {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    e.take_notices()
}

#[test]
fn drop_if_exists_notices() {
    let mut e = Engine::new();
    assert_eq!(
        notices_of(&mut e, "DROP TABLE IF EXISTS missing_tbl"),
        vec!["table \"missing_tbl\" does not exist, skipping"]
    );
    assert_eq!(
        notices_of(&mut e, "DROP INDEX IF EXISTS missing_idx"),
        vec!["index \"missing_idx\" does not exist, skipping"]
    );
    assert_eq!(
        notices_of(&mut e, "DROP VIEW IF EXISTS missing_view"),
        vec!["view \"missing_view\" does not exist, skipping"]
    );
    assert_eq!(
        notices_of(&mut e, "DROP SEQUENCE IF EXISTS missing_seq"),
        vec!["sequence \"missing_seq\" does not exist, skipping"]
    );
    assert_eq!(
        notices_of(&mut e, "DROP SCHEMA IF EXISTS missing_schema"),
        vec!["schema \"missing_schema\" does not exist, skipping"]
    );
    assert_eq!(
        notices_of(&mut e, "DROP TYPE IF EXISTS missing_type"),
        vec!["type \"missing_type\" does not exist, skipping"]
    );
}

#[test]
fn create_if_not_exists_notices() {
    let mut e = Engine::new();
    assert!(notices_of(&mut e, "CREATE TABLE nt1(a int, b text)").is_empty());
    // An index / sequence is a relation, so PG says "relation".
    assert_eq!(
        notices_of(&mut e, "CREATE TABLE IF NOT EXISTS nt1(a int)"),
        vec!["relation \"nt1\" already exists, skipping"]
    );
    assert!(notices_of(&mut e, "CREATE INDEX nt1_idx ON nt1(a)").is_empty());
    assert_eq!(
        notices_of(&mut e, "CREATE INDEX IF NOT EXISTS nt1_idx ON nt1(a)"),
        vec!["relation \"nt1_idx\" already exists, skipping"]
    );
    assert!(notices_of(&mut e, "CREATE SEQUENCE nt_seq").is_empty());
    assert_eq!(
        notices_of(&mut e, "CREATE SEQUENCE IF NOT EXISTS nt_seq"),
        vec!["relation \"nt_seq\" already exists, skipping"]
    );
}

#[test]
fn alter_table_column_notices() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE nt2(a int, b text)").unwrap();
    assert_eq!(
        notices_of(&mut e, "ALTER TABLE nt2 DROP COLUMN IF EXISTS missing_col"),
        vec!["column \"missing_col\" of relation \"nt2\" does not exist, skipping"]
    );
    assert_eq!(
        notices_of(&mut e, "ALTER TABLE nt2 ADD COLUMN IF NOT EXISTS a int"),
        vec!["column \"a\" of relation \"nt2\" already exists, skipping"]
    );
}

#[test]
fn notices_are_per_statement() {
    let mut e = Engine::new();
    e.execute("DROP TABLE IF EXISTS gone").unwrap();
    // A following statement that raises none must not inherit the last
    // statement's notice — the buffer clears on every execute.
    assert!(notices_of(&mut e, "CREATE TABLE nt3(a int)").is_empty());
}
