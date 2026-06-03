//! v7.9.8 — INSERT ... ON CONFLICT (col) DO NOTHING execution.

use spg_engine::{Engine, QueryResult};

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
        _ => 0,
    }
}

#[test]
fn do_nothing_skips_existing_keys() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL, name TEXT)",
        "CREATE INDEX u_pk ON u (id)",
        "INSERT INTO u VALUES (1, 'alice')",
    ]);
    // The second INSERT collides on (id=1) → skip; row count unchanged.
    eng.execute("INSERT INTO u VALUES (1, 'duplicate') ON CONFLICT (id) DO NOTHING")
        .unwrap();
    assert_eq!(count(&mut eng, "SELECT id FROM u"), 1);
}

#[test]
fn do_nothing_inserts_when_no_conflict() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "INSERT INTO u VALUES (1)",
    ]);
    eng.execute("INSERT INTO u VALUES (2) ON CONFLICT (id) DO NOTHING")
        .unwrap();
    assert_eq!(count(&mut eng, "SELECT id FROM u"), 2);
}

#[test]
fn do_nothing_with_mixed_batch() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "INSERT INTO u VALUES (1)",
    ]);
    // Of (1), (2), (3): (1) collides; 2 and 3 new.
    eng.execute(
        "INSERT INTO u VALUES (1), (2), (3) ON CONFLICT (id) DO NOTHING",
    )
    .unwrap();
    assert_eq!(count(&mut eng, "SELECT id FROM u"), 3);
}

#[test]
fn do_nothing_dedups_within_batch() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
    ]);
    // Two rows with id=5 in one VALUES list — second one skipped.
    eng.execute("INSERT INTO u VALUES (5), (5) ON CONFLICT (id) DO NOTHING")
        .unwrap();
    assert_eq!(count(&mut eng, "SELECT id FROM u"), 1);
}

#[test]
fn do_nothing_without_target_uses_first_btree_index() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "INSERT INTO u VALUES (10)",
    ]);
    // No explicit target — engine picks the PK index.
    eng.execute("INSERT INTO u VALUES (10) ON CONFLICT DO NOTHING")
        .unwrap();
    assert_eq!(count(&mut eng, "SELECT id FROM u"), 1);
}

#[test]
fn do_nothing_with_unknown_target_is_rejected() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
    ]);
    let r = eng.execute("INSERT INTO u VALUES (1) ON CONFLICT (ghost) DO NOTHING");
    assert!(r.is_err());
}

#[test]
fn do_nothing_with_returning_only_returns_inserted_rows() {
    // PG behaviour: RETURNING after ON CONFLICT DO NOTHING returns
    // the rows that were actually inserted (skipping skipped ones).
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL, name TEXT)",
        "CREATE INDEX u_pk ON u (id)",
        "INSERT INTO u VALUES (1, 'existing')",
    ]);
    let r = eng
        .execute(
            "INSERT INTO u VALUES (1, 'dup'), (2, 'new') ON CONFLICT (id) DO NOTHING RETURNING id",
        )
        .unwrap();
    let rows = match r {
        QueryResult::Rows { rows, .. } => rows,
        _ => panic!("expected Rows"),
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], spg_storage::Value::Int(2));
}
