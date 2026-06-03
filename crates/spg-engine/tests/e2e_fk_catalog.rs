//! v7.6.1 — FK resolution at CREATE TABLE time + catalog round-trip.
//! Enforcement at INSERT / DELETE / UPDATE lands in v7.6.2+; this
//! file only pins the schema-side wiring.

use spg_engine::{Engine, EngineError, QueryResult};

/// Pull the committed catalog out of an Engine via the public
/// snapshot path. v7.6.1 tests only inspect schema; full
/// round-trip stability of FK bytes lives in
/// `fk_round_trips_through_catalog_serialise`.
fn snapshot_catalog(eng: &Engine) -> spg_storage::Catalog {
    let bytes = eng.snapshot();
    // snapshot() returns either a bare Catalog (when users /
    // publications / subscriptions / statistics are all empty —
    // which is true for these tests) or an envelope. Bare-catalog
    // bytes deserialize directly; on envelope failure we'd fall
    // back, but for these tests we can assert the bare path.
    spg_storage::Catalog::deserialize(&bytes).expect("bare catalog deserialises")
}

fn engine_with(sqls: &[&str]) -> Engine {
    let mut eng = Engine::new();
    for sql in sqls {
        let r = eng.execute(sql).unwrap_or_else(|e| panic!("{sql:?}: {e:?}"));
        assert!(
            matches!(r, QueryResult::CommandOk { .. }),
            "expected CommandOk for {sql:?}"
        );
    }
    eng
}

#[test]
fn fk_with_btree_parent_index_succeeds() {
    let eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL, name TEXT)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL REFERENCES u(id))",
    ]);
    let cat = snapshot_catalog(&eng);
    let o = cat.get("o").expect("table o");
    assert_eq!(o.schema().foreign_keys.len(), 1);
    let fk = &o.schema().foreign_keys[0];
    assert_eq!(fk.parent_table, "u");
    assert_eq!(fk.local_columns, vec![1]);
    assert_eq!(fk.parent_columns, vec![0]);
}

#[test]
fn fk_without_parent_index_is_rejected() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE u (id INT NOT NULL, name TEXT)").unwrap();
    let r = eng.execute("CREATE TABLE o (uid INT NOT NULL REFERENCES u(id))");
    assert!(matches!(r, Err(EngineError::Unsupported(_))));
}

#[test]
fn fk_against_missing_parent_table_is_rejected() {
    let mut eng = Engine::new();
    let r = eng.execute(
        "CREATE TABLE o (uid INT NOT NULL, FOREIGN KEY (uid) REFERENCES ghost(id))",
    );
    assert!(matches!(r, Err(EngineError::Storage(_))));
}

#[test]
fn fk_against_unknown_parent_column_is_rejected() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
    ]);
    let r = eng.execute(
        "CREATE TABLE o (uid INT NOT NULL, FOREIGN KEY (uid) REFERENCES u(ghost))",
    );
    assert!(matches!(r, Err(EngineError::Unsupported(_))));
}

#[test]
fn fk_table_level_with_cascade() {
    let eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL, \
         FOREIGN KEY (uid) REFERENCES u(id) ON DELETE CASCADE ON UPDATE SET NULL)",
    ]);
    let cat = snapshot_catalog(&eng);
    let fk = &cat.get("o").unwrap().schema().foreign_keys[0];
    assert_eq!(fk.on_delete, spg_storage::FkAction::Cascade);
    assert_eq!(fk.on_update, spg_storage::FkAction::SetNull);
}

#[test]
fn fk_default_parent_columns_uses_pk_index() {
    // No parent column list → engine picks the parent's BTree index column.
    let eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (uid INT NOT NULL REFERENCES u)",
    ]);
    let cat = snapshot_catalog(&eng);
    let fk = &cat.get("o").unwrap().schema().foreign_keys[0];
    assert_eq!(fk.parent_columns, vec![0]);
}

#[test]
fn fk_self_referencing_with_explicit_parent_column() {
    let eng = engine_with(&[
        "CREATE TABLE org (id INT NOT NULL, parent_id INT, \
         FOREIGN KEY (parent_id) REFERENCES org(id))",
    ]);
    let cat = snapshot_catalog(&eng);
    let fk = &cat.get("org").unwrap().schema().foreign_keys[0];
    assert_eq!(fk.parent_table, "org");
    assert_eq!(fk.local_columns, vec![1]);
    assert_eq!(fk.parent_columns, vec![0]);
}

#[test]
fn fk_round_trips_through_catalog_serialise() {
    let eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (uid INT NOT NULL REFERENCES u(id) ON DELETE CASCADE)",
    ]);
    let cat = snapshot_catalog(&eng);
    let bytes = cat.serialize();
    let cat2 = spg_storage::Catalog::deserialize(&bytes).expect("round-trip");
    let fk = &cat2.get("o").unwrap().schema().foreign_keys[0];
    assert_eq!(fk.parent_table, "u");
    assert_eq!(fk.local_columns, vec![0]);
    assert_eq!(fk.parent_columns, vec![0]);
    assert_eq!(fk.on_delete, spg_storage::FkAction::Cascade);
}

#[test]
fn multiple_fks_persist() {
    let eng = engine_with(&[
        "CREATE TABLE p1 (id INT NOT NULL)",
        "CREATE INDEX p1_pk ON p1 (id)",
        "CREATE TABLE p2 (id INT NOT NULL)",
        "CREATE INDEX p2_pk ON p2 (id)",
        "CREATE TABLE c (a INT NOT NULL REFERENCES p1(id), \
         b INT NOT NULL REFERENCES p2(id) ON DELETE SET NULL)",
    ]);
    let cat = snapshot_catalog(&eng);
    let bytes = cat.serialize();
    let cat2 = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let fks = &cat2.get("c").unwrap().schema().foreign_keys;
    assert_eq!(fks.len(), 2);
    assert_eq!(fks[0].parent_table, "p1");
    assert_eq!(fks[1].parent_table, "p2");
    assert_eq!(fks[1].on_delete, spg_storage::FkAction::SetNull);
}
