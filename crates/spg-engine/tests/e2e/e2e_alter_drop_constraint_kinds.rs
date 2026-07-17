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
    // v7.39 (read01 round 48) — DROP now resolves through the very
    // synthesiser pg_constraint reports from, so the name that works is the
    // name the catalog shows: PG's `<table>_<cols…>_key`. (The old
    // `<table>_uniq<idx>` this pinned was never printed by any view — DROP
    // and pg_constraint had simply disagreed; oracle-verified as t_a_b_key.)
    e.execute("ALTER TABLE t DROP CONSTRAINT t_a_b_key")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 1)").unwrap();
    e.execute("INSERT INTO t VALUES (1, 1)").unwrap();
}

#[test]
fn drop_constraint_check_via_synth_name() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, status TEXT, CHECK (status IN ('a', 'b')))")
        .unwrap();
    // v7.39 (read01 round 48) — same fix: PG's `<table>_<col>_check` is what
    // pg_constraint prints and what DROP now accepts (oracle-verified).
    e.execute("ALTER TABLE t DROP CONSTRAINT t_status_check")
        .unwrap();
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
    // v7.39 (read01 round 47) — PG wording (42704 at the wire).
    assert!(matches!(
        err,
        EngineError::Unsupported(ref s)
            if s.contains("constraint \"nope\" of relation \"t\" does not exist")
    ));
}
