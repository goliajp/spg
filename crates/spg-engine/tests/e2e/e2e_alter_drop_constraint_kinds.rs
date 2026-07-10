//! v7.37.18 (18.17) — ALTER TABLE DROP CONSTRAINT widened from
//! FK-only to FK / PK / UNIQUE / CHECK. PG dumps emit
//! `DROP CONSTRAINT t_pkey` for table reshapes; SPG used to
//! reject those because the dispatcher only knew about FKs.

use spg_engine::{Engine, EngineError};

#[test]
fn drop_constraint_primary_key() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT, PRIMARY KEY (id))")
        .unwrap();
    e.execute("ALTER TABLE t DROP CONSTRAINT t_pkey").unwrap();
    // After PK drop, inserting two rows with the same id no
    // longer surfaces a uniqueness violation.
    e.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
    e.execute("INSERT INTO t VALUES (1, 'b')").unwrap();
}

#[test]
fn drop_constraint_unique_via_synth_name() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a INT NOT NULL, b INT NOT NULL, UNIQUE (a, b))")
        .unwrap();
    // pg_constraint synthesises composite UC names as
    // `<table>_uniq<idx>` — idx is 0 for the first UC.
    e.execute("ALTER TABLE t DROP CONSTRAINT t_uniq0").unwrap();
    e.execute("INSERT INTO t VALUES (1, 1)").unwrap();
    e.execute("INSERT INTO t VALUES (1, 1)").unwrap();
}

#[test]
fn drop_constraint_check_via_synth_name() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, status TEXT, CHECK (status IN ('a', 'b')))")
        .unwrap();
    // pg_constraint synthesises CHECK names as
    // `<table>_check<idx>` — idx 0 for the first.
    e.execute("ALTER TABLE t DROP CONSTRAINT t_check0").unwrap();
    // The CHECK is gone, so a previously-rejected value lands.
    e.execute("INSERT INTO t VALUES (1, 'z')").unwrap();
}

#[test]
fn drop_constraint_if_exists_silently_succeeds() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    e.execute("ALTER TABLE t DROP CONSTRAINT IF EXISTS nope")
        .unwrap();
}

#[test]
fn drop_constraint_unknown_without_if_exists_errors() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    let err = e.execute("ALTER TABLE t DROP CONSTRAINT nope").unwrap_err();
    assert!(matches!(err, EngineError::Unsupported(ref s) if s.contains("no constraint named")));
}
