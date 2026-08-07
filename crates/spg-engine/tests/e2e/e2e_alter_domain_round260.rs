//! v7.39 (round 260) — `ALTER DOMAIN`, swept against live PG18.4
//! (2026-07-20). Every form used to be swallowed by the parser's
//! pg_dump no-op arm: the statement reported SUCCESS and changed
//! nothing, so a migration that dropped a constraint kept rejecting the
//! data it had just been told to accept. Silent acceptance is worse
//! than an outright refusal — the caller has no way to notice.
//!
//! Implemented here: ADD [CONSTRAINT name] CHECK, DROP CONSTRAINT
//! [IF EXISTS], SET / DROP DEFAULT, SET / DROP NOT NULL, RENAME TO,
//! plus PG's error wordings for a missing domain and a missing or
//! duplicate constraint.
//!
//! Supporting DROP CONSTRAINT by name required domain checks to CARRY
//! their names (`DomainDef.checks: Vec<DomainCheck>`, catalog
//! FILE_VERSION 75). PG's auto-naming, probed: `<domain>_check`, then
//! `_check1`, `_check2`, …; a violation reports the constraint that
//! actually failed, which now differs from the domain name once a
//! domain has more than one check.
//!
//! Recorded residual, probed: `SET`/`DROP DEFAULT` does not reach
//! columns of tables that ALREADY exist. The domain's default is
//! snapshotted onto the column at CREATE TABLE, and the INSERT-time
//! resolver (`resolve_column_default_free`) has no catalog handle to
//! re-read the domain. New tables and all casts see the change.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(x) => format!("{x}"),
        Ok(ok) => panic!("{sql}: expected an error, got {ok:?}"),
    }
}

#[test]
fn add_and_drop_constraint_take_effect() {
    let mut e = Engine::new();
    e.execute("CREATE DOMAIN ad AS int CHECK (VALUE > 0)")
        .unwrap();
    let got = err(&mut e, "SELECT (-1)::ad");
    assert!(
        got.contains("violates check constraint \"ad_check\""),
        "{got}"
    );
    // Dropping it actually lets the value through — this used to report
    // success and keep rejecting.
    e.execute("ALTER DOMAIN ad DROP CONSTRAINT ad_check")
        .unwrap();
    assert_eq!(one(&mut e, "SELECT (-1)::ad"), "-1");
    // …and a re-added one takes effect immediately.
    e.execute("ALTER DOMAIN ad ADD CONSTRAINT ad_check CHECK (VALUE > 10)")
        .unwrap();
    let got = err(&mut e, "SELECT 5::ad");
    assert!(
        got.contains("violates check constraint \"ad_check\""),
        "{got}"
    );
    assert_eq!(one(&mut e, "SELECT 20::ad"), "20");
    // A second, explicitly named constraint reports ITS name.
    e.execute("ALTER DOMAIN ad ADD CONSTRAINT c2 CHECK (VALUE < 1000)")
        .unwrap();
    let got = err(&mut e, "SELECT 2000::ad");
    assert!(got.contains("violates check constraint \"c2\""), "{got}");
    assert_eq!(one(&mut e, "SELECT 500::ad"), "500");
}

#[test]
fn the_error_wordings_are_pgs() {
    let mut e = Engine::new();
    e.execute("CREATE DOMAIN ad AS int CHECK (VALUE > 0)")
        .unwrap();
    let got = err(&mut e, "ALTER DOMAIN ad DROP CONSTRAINT nosuch");
    assert!(
        got.contains("constraint \"nosuch\" of domain \"ad\" does not exist"),
        "{got}"
    );
    // IF EXISTS is a no-op instead.
    e.execute("ALTER DOMAIN ad DROP CONSTRAINT IF EXISTS nosuch")
        .unwrap();
    let got = err(&mut e, "ALTER DOMAIN nosuchdomain DROP CONSTRAINT x");
    assert!(
        got.contains("type \"nosuchdomain\" does not exist"),
        "{got}"
    );
    let got = err(
        &mut e,
        "ALTER DOMAIN ad ADD CONSTRAINT ad_check CHECK (VALUE > 0)",
    );
    assert!(
        got.contains("constraint \"ad_check\" for domain \"ad\" already exists"),
        "{got}"
    );
}

#[test]
fn set_not_null_validates_existing_data() {
    let mut e = Engine::new();
    e.execute("CREATE DOMAIN zq7 AS int").unwrap();
    e.execute("CREATE TABLE zq7t (id int, v zq7)").unwrap();
    e.execute("INSERT INTO zq7t VALUES (1, 5)").unwrap();
    e.execute("INSERT INTO zq7t VALUES (2, NULL)").unwrap();
    // PG refuses while a column of this domain holds NULLs.
    let got = err(&mut e, "ALTER DOMAIN zq7 SET NOT NULL");
    assert!(
        got.contains("column \"v\" of table \"zq7t\" contains null values"),
        "{got}"
    );
    assert_eq!(one(&mut e, "SELECT NULL::zq7"), "NULL");
    // Once the NULL is gone it succeeds, and the domain rejects NULLs.
    e.execute("DELETE FROM zq7t WHERE id = 2").unwrap();
    e.execute("ALTER DOMAIN zq7 SET NOT NULL").unwrap();
    let got = err(&mut e, "SELECT NULL::zq7");
    assert!(
        got.contains("domain zq7 does not allow null values"),
        "{got}"
    );
    // …and DROP NOT NULL puts it back.
    e.execute("ALTER DOMAIN zq7 DROP NOT NULL").unwrap();
    assert_eq!(one(&mut e, "SELECT NULL::zq7"), "NULL");
}

#[test]
fn default_and_rename_take_effect() {
    let mut e = Engine::new();
    e.execute("CREATE DOMAIN wd AS int").unwrap();
    e.execute("ALTER DOMAIN wd SET DEFAULT 99").unwrap();
    // A table created AFTER the ALTER sees the new default.
    e.execute("CREATE TABLE wdt (id int, v wd)").unwrap();
    e.execute("INSERT INTO wdt (id) VALUES (1)").unwrap();
    assert_eq!(one(&mut e, "SELECT v FROM wdt WHERE id=1"), "99");
    e.execute("ALTER DOMAIN wd DROP DEFAULT").unwrap();
    e.execute("CREATE TABLE wdt2 (id int, v wd)").unwrap();
    e.execute("INSERT INTO wdt2 (id) VALUES (1)").unwrap();
    assert_eq!(one(&mut e, "SELECT v FROM wdt2 WHERE id=1"), "NULL");
    // RENAME moves the type to its new name.
    e.execute("CREATE DOMAIN rn AS int CHECK (VALUE > 0)")
        .unwrap();
    e.execute("ALTER DOMAIN rn RENAME TO rn2").unwrap();
    assert_eq!(one(&mut e, "SELECT 5::rn2"), "5");
    let got = err(&mut e, "SELECT (-5)::rn2");
    assert!(
        got.contains("violates check constraint \"rn_check\""),
        "{got}"
    );
    // The old name is gone.
    assert!(e.execute("SELECT 5::rn").is_err());
}

#[test]
fn multiple_unnamed_checks_get_pgs_auto_names() {
    let mut e = Engine::new();
    e.execute("CREATE DOMAIN nm AS int CHECK (VALUE > 0) CHECK (VALUE < 100)")
        .unwrap();
    // `<domain>_check`, then `_check1` (probed).
    let got = err(&mut e, "SELECT 0::nm");
    assert!(
        got.contains("violates check constraint \"nm_check\""),
        "{got}"
    );
    let got = err(&mut e, "SELECT 500::nm");
    assert!(
        got.contains("violates check constraint \"nm_check1\""),
        "{got}"
    );
    // An unnamed ALTER-added one continues the sequence.
    e.execute("ALTER DOMAIN nm ADD CHECK (VALUE <> 7)").unwrap();
    let got = err(&mut e, "SELECT 7::nm");
    assert!(
        got.contains("violates check constraint \"nm_check2\""),
        "{got}"
    );
}
