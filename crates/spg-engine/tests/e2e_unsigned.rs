//! v7.17.0 Phase 4.4 — MySQL UNSIGNED range enforcement.

use spg_engine::Engine;

#[test]
fn unsigned_int_rejects_negative_insert() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT UNSIGNED NOT NULL)")
        .unwrap();
    let r = e.execute("INSERT INTO t VALUES (-1)");
    assert!(r.is_err());
}

#[test]
fn unsigned_int_accepts_positive_insert() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT UNSIGNED NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1), (42), (1000000)")
        .unwrap();
}

#[test]
fn unsigned_int_accepts_zero() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT UNSIGNED NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (0)").unwrap();
}

#[test]
fn signed_int_still_accepts_negative() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO t VALUES (-1)").unwrap();
}

#[test]
fn unsigned_bigint_rejects_negative() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id BIGINT UNSIGNED NOT NULL)")
        .unwrap();
    let r = e.execute("INSERT INTO t VALUES (-100)");
    assert!(r.is_err());
}

#[test]
fn unsigned_smallint_rejects_negative() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id SMALLINT UNSIGNED NOT NULL)")
        .unwrap();
    let r = e.execute("INSERT INTO t VALUES (-1)");
    assert!(r.is_err());
}

#[test]
fn update_to_negative_on_unsigned_errors() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT UNSIGNED NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
    let r = e.execute("UPDATE t SET id = -1 WHERE name = 'a'");
    assert!(r.is_err());
}

#[test]
fn unsigned_flag_persists_through_catalog_roundtrip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT UNSIGNED NOT NULL)")
        .unwrap();
    let snapshot = e.catalog().serialize();
    let reloaded = spg_storage::Catalog::deserialize(&snapshot).expect("roundtrip");
    let table = reloaded.get("t").unwrap();
    let col = table
        .schema()
        .columns
        .iter()
        .find(|c| c.name == "id")
        .unwrap();
    assert!(col.is_unsigned);
}

#[test]
fn no_unsigned_column_means_no_appendix_bytes() {
    // Negative regression: ensure a table with no UNSIGNED columns
    // still round-trips through the new FILE_VERSION 35 codec.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    let snapshot = e.catalog().serialize();
    let reloaded = spg_storage::Catalog::deserialize(&snapshot).expect("roundtrip");
    let table = reloaded.get("t").unwrap();
    let col = table
        .schema()
        .columns
        .iter()
        .find(|c| c.name == "id")
        .unwrap();
    assert!(!col.is_unsigned);
}
