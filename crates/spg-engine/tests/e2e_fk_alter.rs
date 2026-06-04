//! v7.6.8 — ALTER TABLE ADD/DROP CONSTRAINT.

use spg_engine::{Engine, EngineError, QueryResult};

fn engine_with(sqls: &[&str]) -> Engine {
    let mut eng = Engine::new();
    for sql in sqls {
        let r = eng
            .execute(sql)
            .unwrap_or_else(|e| panic!("setup {sql:?}: {e:?}"));
        assert!(matches!(r, QueryResult::CommandOk { .. }), "{sql:?}");
    }
    eng
}

#[test]
fn add_constraint_on_compatible_existing_data_succeeds() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL)",
        "INSERT INTO u VALUES (1), (2)",
        "INSERT INTO o VALUES (10, 1), (11, 2)",
    ]);
    eng.execute("ALTER TABLE o ADD CONSTRAINT fk_user FOREIGN KEY (uid) REFERENCES u(id)")
        .unwrap();
    // Subsequent insert violating the new FK is rejected.
    let r = eng.execute("INSERT INTO o VALUES (12, 99)");
    assert!(
        matches!(r, Err(EngineError::Unsupported(ref s)) if s.contains("FOREIGN KEY violation"))
    );
}

#[test]
fn add_constraint_against_violating_existing_data_is_rejected() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL)",
        "INSERT INTO u VALUES (1)",
        // 99 doesn't exist in u — installing the FK should fail.
        "INSERT INTO o VALUES (10, 99)",
    ]);
    let r = eng.execute("ALTER TABLE o ADD CONSTRAINT fk_user FOREIGN KEY (uid) REFERENCES u(id)");
    assert!(
        matches!(r, Err(EngineError::Unsupported(ref s)) if s.contains("FOREIGN KEY violation"))
    );
}

#[test]
fn drop_constraint_removes_enforcement() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL, \
         CONSTRAINT fk_user FOREIGN KEY (uid) REFERENCES u(id))",
        "INSERT INTO u VALUES (1)",
        "INSERT INTO o VALUES (10, 1)",
    ]);
    // FK active — this would normally be rejected.
    assert!(eng.execute("INSERT INTO o VALUES (11, 99)").is_err());
    // Drop the constraint.
    eng.execute("ALTER TABLE o DROP CONSTRAINT fk_user")
        .unwrap();
    // Now the violation is no longer caught.
    eng.execute("INSERT INTO o VALUES (11, 99)").unwrap();
}

#[test]
fn drop_unknown_constraint_is_rejected() {
    let mut eng = engine_with(&["CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL)"]);
    let r = eng.execute("ALTER TABLE o DROP CONSTRAINT ghost");
    assert!(matches!(r, Err(EngineError::Unsupported(ref s)) if s.contains("no FK named")));
}

#[test]
fn duplicate_constraint_name_is_rejected() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, a INT NOT NULL, b INT NOT NULL, \
         CONSTRAINT shared FOREIGN KEY (a) REFERENCES u(id))",
    ]);
    let r = eng.execute("ALTER TABLE o ADD CONSTRAINT shared FOREIGN KEY (b) REFERENCES u(id)");
    assert!(matches!(r, Err(EngineError::Unsupported(ref s)) if s.contains("already exists")));
}

#[test]
fn add_constraint_survives_catalog_snapshot_round_trip() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL)",
        "INSERT INTO u VALUES (1)",
        "INSERT INTO o VALUES (10, 1)",
    ]);
    eng.execute("ALTER TABLE o ADD CONSTRAINT fk_user FOREIGN KEY (uid) REFERENCES u(id)")
        .unwrap();
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    assert_eq!(cat.get("o").unwrap().schema().foreign_keys.len(), 1);
    assert_eq!(
        cat.get("o").unwrap().schema().foreign_keys[0].name,
        Some("fk_user".into())
    );
}
