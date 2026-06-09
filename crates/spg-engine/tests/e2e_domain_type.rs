//! v7.17.0 Phase 1.5 — CREATE DOMAIN with CHECK enforcement.
//! Validates the catalog round-trip + CREATE TABLE column
//! binding + INSERT-time domain CHECK / NOT NULL enforcement.

use spg_engine::Engine;

#[test]
fn create_domain_then_use_in_column() {
    let mut e = Engine::new();
    e.execute("CREATE DOMAIN positive_int AS INT CHECK (value > 0)")
        .unwrap();
    e.execute("CREATE TABLE t (id INT NOT NULL, n positive_int)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 5)").unwrap();
    e.execute("INSERT INTO t VALUES (2, 100)").unwrap();
}

#[test]
fn domain_check_rejects_bad_value_at_insert() {
    let mut e = Engine::new();
    e.execute("CREATE DOMAIN positive_int AS INT CHECK (value > 0)")
        .unwrap();
    e.execute("CREATE TABLE t (id INT NOT NULL, n positive_int)")
        .unwrap();
    let err = e.execute("INSERT INTO t VALUES (1, -5)");
    assert!(err.is_err(), "DOMAIN CHECK should reject negative");
}

#[test]
fn domain_not_null_propagates_to_column() {
    let mut e = Engine::new();
    e.execute("CREATE DOMAIN required_text AS TEXT NOT NULL").unwrap();
    e.execute("CREATE TABLE t (id INT NOT NULL, label required_text)")
        .unwrap();
    // Domain's NOT NULL makes the column non-nullable.
    let err = e.execute("INSERT INTO t VALUES (1, NULL)");
    assert!(err.is_err(), "DOMAIN NOT NULL should reject NULL");
}

#[test]
fn domain_multiple_checks_all_enforced() {
    let mut e = Engine::new();
    e.execute(
        "CREATE DOMAIN small_pos AS INT CHECK (value > 0) CHECK (value < 100)",
    )
    .unwrap();
    e.execute("CREATE TABLE t (id INT NOT NULL, n small_pos)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 50)").unwrap();
    let err1 = e.execute("INSERT INTO t VALUES (2, 0)");
    assert!(err1.is_err());
    let err2 = e.execute("INSERT INTO t VALUES (3, 200)");
    assert!(err2.is_err());
}

#[test]
fn drop_domain_removes_it() {
    let mut e = Engine::new();
    e.execute("CREATE DOMAIN d AS INT CHECK (value > 0)").unwrap();
    e.execute("DROP DOMAIN d").unwrap();
    // Re-create allowed.
    e.execute("CREATE DOMAIN d AS INT").unwrap();
}

#[test]
fn drop_domain_if_exists_silent_on_missing() {
    let mut e = Engine::new();
    e.execute("DROP DOMAIN IF EXISTS missing").unwrap();
}

#[test]
fn duplicate_create_domain_errors() {
    let mut e = Engine::new();
    e.execute("CREATE DOMAIN d AS INT").unwrap();
    let err = e.execute("CREATE DOMAIN d AS TEXT");
    assert!(err.is_err());
}

#[test]
fn null_passes_domain_check_per_pg() {
    // PG semantics: NULL passes a CHECK constraint (rejects only
    // on definite-false). The DOMAIN's CHECK should follow.
    let mut e = Engine::new();
    e.execute("CREATE DOMAIN d AS INT CHECK (value > 0)").unwrap();
    e.execute("CREATE TABLE t (id INT NOT NULL, n d)").unwrap();
    e.execute("INSERT INTO t VALUES (1, NULL)").unwrap();
}

#[test]
fn domain_round_trips_catalog() {
    let mut e = Engine::new();
    e.execute("CREATE DOMAIN positive_int AS INT CHECK (value > 0)")
        .unwrap();
    e.execute("CREATE TABLE t (id INT NOT NULL, n positive_int)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 7)").unwrap();
    let snapshot = e.catalog().serialize();
    let restored = spg_storage::Catalog::deserialize(&snapshot).expect("round-trip");
    let dom = restored
        .domain_types()
        .get("positive_int")
        .expect("domain persisted");
    assert_eq!(dom.checks.len(), 1);
    let col = restored
        .get("t")
        .expect("table")
        .schema()
        .columns
        .iter()
        .find(|c| c.name == "n")
        .expect("column");
    assert_eq!(col.user_domain_type.as_deref(), Some("positive_int"));
}

#[test]
fn domain_text_with_check_on_length() {
    let mut e = Engine::new();
    e.execute("CREATE DOMAIN short_label AS TEXT CHECK (length(value) <= 5)")
        .unwrap();
    e.execute("CREATE TABLE t (id INT NOT NULL, l short_label)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 'hello')").unwrap();
    let err = e.execute("INSERT INTO t VALUES (2, 'too long')");
    assert!(err.is_err(), "DOMAIN should reject TEXT > 5 chars");
}

#[test]
fn domain_collision_with_existing_object_rejected() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE foo (id INT NOT NULL)").unwrap();
    let err = e.execute("CREATE DOMAIN foo AS INT");
    assert!(err.is_err());
}
