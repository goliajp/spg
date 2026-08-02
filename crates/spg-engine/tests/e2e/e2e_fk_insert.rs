//! v7.6.2 — INSERT-side FK enforcement.
//!
//! v7.39 (round 695) — and, since the file already held the DELETE-side
//! placeholder, every ON DELETE action, verified against PG18.

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

fn count(eng: &mut Engine, sql: &str) -> usize {
    match eng.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.len(),
        _ => panic!("expected Rows"),
    }
}

#[test]
fn insert_matching_parent_succeeds() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL REFERENCES u(id))",
        "INSERT INTO u VALUES (1), (2)",
    ]);
    eng.execute("INSERT INTO o VALUES (10, 1), (11, 2)")
        .unwrap();
    assert_eq!(count(&mut eng, "SELECT id FROM o"), 2);
}

#[test]
fn insert_missing_parent_is_rejected() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL REFERENCES u(id))",
        "INSERT INTO u VALUES (1)",
    ]);
    let r = eng.execute("INSERT INTO o VALUES (10, 99)");
    assert!(
        matches!(r, Err(EngineError::Unsupported(ref s)) if s.to_lowercase().contains("foreign key"))
    );
    // No row persisted.
    assert_eq!(count(&mut eng, "SELECT id FROM o"), 0);
}

#[test]
fn insert_with_null_fk_column_skips_check() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT REFERENCES u(id))",
    ]);
    eng.execute("INSERT INTO o VALUES (10, NULL)").unwrap();
    assert_eq!(count(&mut eng, "SELECT id FROM o"), 1);
}

#[test]
fn batch_insert_atomically_rejected_if_any_row_violates() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL REFERENCES u(id))",
        "INSERT INTO u VALUES (1)",
    ]);
    let r = eng.execute("INSERT INTO o VALUES (10, 1), (11, 99)");
    assert!(r.is_err());
    // First (valid) row is also NOT inserted — enforcement is
    // pre-insert, so the batch is atomic at FK granularity.
    assert_eq!(count(&mut eng, "SELECT id FROM o"), 0);
}

#[test]
fn insert_to_table_without_fk_is_unaffected() {
    let mut eng = engine_with(&[
        "CREATE TABLE plain (id INT NOT NULL, name TEXT)",
        "INSERT INTO plain VALUES (1, 'a'), (2, 'b')",
    ]);
    assert_eq!(count(&mut eng, "SELECT id FROM plain"), 2);
}

#[test]
fn round695_every_on_delete_action_behaves_as_pg18_does() {
    // v7.39 (round 695) — this test was `cascade_action_stored_but_not_yet
    // _enforced_on_delete`, written for v7.6.2 to pin "DELETE on the parent
    // removes it without touching the child, and v7.6.4 will change that".
    // v7.6.4 shipped. What the test had become was two `let _ =` statements
    // asserting nothing at all, under a name that told a reader the feature
    // was missing — the F31 shape exactly, and the worst variant of it,
    // because a vacuous test cannot even go red when the claim stops
    // holding.
    //
    // All four actions, each verified against PG18.
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT PRIMARY KEY)",
        "CREATE TABLE o (id INT, uid INT REFERENCES u(id) ON DELETE CASCADE)",
        "INSERT INTO u VALUES (1),(2)",
        "INSERT INTO o VALUES (10, 1), (11, 2)",
    ]);
    eng.execute("DELETE FROM u WHERE id = 1").unwrap();
    // CASCADE took the child row with it; the unrelated one stayed.
    assert_eq!(ids(&mut eng, "SELECT id FROM o ORDER BY id"), "11");

    // RESTRICT refuses, and PG's wording for RESTRICT is its OWN — it is not
    // NO ACTION's. The two differ in when they fire, so the word matters.
    let mut eng = engine_with(&[
        "CREATE TABLE p (id INT PRIMARY KEY)",
        "CREATE TABLE r (id INT, pid INT REFERENCES p(id) ON DELETE RESTRICT)",
        "INSERT INTO p VALUES (1)",
        "INSERT INTO r VALUES (10, 1)",
    ]);
    let err = format!("{}", eng.execute("DELETE FROM p WHERE id = 1").unwrap_err());
    assert!(
        err.contains("violates RESTRICT setting of foreign key constraint"),
        "RESTRICT wording: {err}"
    );
    assert!(err.contains("is referenced from table"), "{err}");
    assert!(!err.contains("is still referenced"), "that is NO ACTION's: {err}");

    // NO ACTION (the default) refuses with the other wording.
    let mut eng = engine_with(&[
        "CREATE TABLE m (id INT PRIMARY KEY)",
        "CREATE TABLE n (id INT, mid INT REFERENCES m(id))",
        "INSERT INTO m VALUES (1)",
        "INSERT INTO n VALUES (10, 1)",
    ]);
    let err = format!("{}", eng.execute("DELETE FROM m WHERE id = 1").unwrap_err());
    assert!(err.contains("violates foreign key constraint"), "{err}");
    assert!(err.contains("is still referenced from table"), "{err}");
    assert!(!err.contains("RESTRICT setting"), "{err}");

    // SET NULL keeps the child row and nulls the reference.
    let mut eng = engine_with(&[
        "CREATE TABLE x (id INT PRIMARY KEY)",
        "CREATE TABLE y (id INT, xid INT REFERENCES x(id) ON DELETE SET NULL)",
        "INSERT INTO x VALUES (1)",
        "INSERT INTO y VALUES (10, 1)",
    ]);
    eng.execute("DELETE FROM x WHERE id = 1").unwrap();
    assert_eq!(ids(&mut eng, "SELECT id FROM y WHERE xid IS NULL"), "10");
}

/// Comma-joined first column, so a row set can be asserted in one line.
fn ids(eng: &mut Engine, sql: &str) -> String {
    match eng.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect::<Vec<_>>()
            .join(","),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn self_referencing_fk_root_insert_works() {
    // Root row references no parent (NULL); subsequent rows can
    // reference earlier rows visible in the same TABLE — but
    // v7.6.2 enforcement is per-batch, so cross-row references in
    // a single multi-VALUES INSERT are NOT yet allowed.
    let mut eng = engine_with(&[
        "CREATE TABLE org (id INT NOT NULL, parent_id INT, \
         FOREIGN KEY (parent_id) REFERENCES org(id))",
        "CREATE INDEX org_pk ON org (id)",
        "INSERT INTO org VALUES (1, NULL)",
        "INSERT INTO org VALUES (2, 1)",
    ]);
    assert_eq!(count(&mut eng, "SELECT id FROM org"), 2);
}
