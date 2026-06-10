//! v7.6.4 — DELETE ON CASCADE.

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

fn rows(eng: &mut Engine, sql: &str) -> usize {
    match eng.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.len(),
        _ => panic!("expected Rows"),
    }
}

#[test]
fn cascade_deletes_referring_child_rows() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL, \
         FOREIGN KEY (uid) REFERENCES u(id) ON DELETE CASCADE)",
        "INSERT INTO u VALUES (1), (2)",
        "INSERT INTO o VALUES (10, 1), (11, 1), (12, 2)",
    ]);
    eng.execute("DELETE FROM u WHERE id = 1").unwrap();
    // u(id=1) gone + o rows referencing 1 (10, 11) gone.
    assert_eq!(rows(&mut eng, "SELECT id FROM u"), 1);
    assert_eq!(rows(&mut eng, "SELECT id FROM o"), 1);
}

#[test]
fn cascade_does_not_touch_unrelated_child_rows() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL, \
         FOREIGN KEY (uid) REFERENCES u(id) ON DELETE CASCADE)",
        "INSERT INTO u VALUES (1), (2)",
        "INSERT INTO o VALUES (10, 1), (11, 2)",
    ]);
    eng.execute("DELETE FROM u WHERE id = 1").unwrap();
    // Only the o row referencing 1 should disappear.
    match eng.execute("SELECT id FROM o").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
        }
        _ => panic!("rows"),
    }
}

#[test]
fn cascade_chain_two_levels_deep() {
    // a → b → c (each level cascades). Deleting a(id=1) should
    // remove the chain.
    let mut eng = engine_with(&[
        "CREATE TABLE a (id INT NOT NULL)",
        "CREATE INDEX a_pk ON a (id)",
        "CREATE TABLE b (id INT NOT NULL, a_id INT NOT NULL, \
         FOREIGN KEY (a_id) REFERENCES a(id) ON DELETE CASCADE)",
        "CREATE INDEX b_pk ON b (id)",
        "CREATE TABLE c (id INT NOT NULL, b_id INT NOT NULL, \
         FOREIGN KEY (b_id) REFERENCES b(id) ON DELETE CASCADE)",
        "INSERT INTO a VALUES (1), (2)",
        "INSERT INTO b VALUES (10, 1), (20, 2)",
        "INSERT INTO c VALUES (100, 10), (200, 20)",
    ]);
    eng.execute("DELETE FROM a WHERE id = 1").unwrap();
    assert_eq!(rows(&mut eng, "SELECT id FROM a"), 1);
    assert_eq!(rows(&mut eng, "SELECT id FROM b"), 1);
    assert_eq!(rows(&mut eng, "SELECT id FROM c"), 1);
}

#[test]
fn cascade_mixed_with_restrict_still_errors_on_restrict_branch() {
    // a has TWO child tables. b cascades, c restricts. Deleting
    // a(id=1) when c references it must fail and undo nothing.
    let mut eng = engine_with(&[
        "CREATE TABLE a (id INT NOT NULL)",
        "CREATE INDEX a_pk ON a (id)",
        "CREATE TABLE b (id INT NOT NULL, a_id INT NOT NULL, \
         FOREIGN KEY (a_id) REFERENCES a(id) ON DELETE CASCADE)",
        "CREATE TABLE c (id INT NOT NULL, a_id INT NOT NULL, \
         FOREIGN KEY (a_id) REFERENCES a(id))", // default RESTRICT
        "INSERT INTO a VALUES (1)",
        "INSERT INTO b VALUES (10, 1)",
        "INSERT INTO c VALUES (100, 1)",
    ]);
    let r = eng.execute("DELETE FROM a WHERE id = 1");
    assert!(
        matches!(r, Err(EngineError::Unsupported(ref s)) if s.contains("FOREIGN KEY violation"))
    );
    // Nothing changed.
    assert_eq!(rows(&mut eng, "SELECT id FROM a"), 1);
    assert_eq!(rows(&mut eng, "SELECT id FROM b"), 1);
    assert_eq!(rows(&mut eng, "SELECT id FROM c"), 1);
}

#[test]
fn cascade_batch_delete_multiple_parents() {
    let mut eng = engine_with(&[
        "CREATE TABLE u (id INT NOT NULL)",
        "CREATE INDEX u_pk ON u (id)",
        "CREATE TABLE o (id INT NOT NULL, uid INT NOT NULL, \
         FOREIGN KEY (uid) REFERENCES u(id) ON DELETE CASCADE)",
        "INSERT INTO u VALUES (1), (2), (3)",
        "INSERT INTO o VALUES (10, 1), (20, 2), (30, 3)",
    ]);
    // Delete u rows 1 and 2 in one statement; cascade should
    // remove o(10) and o(20) but leave o(30).
    eng.execute("DELETE FROM u WHERE id <> 3").unwrap();
    assert_eq!(rows(&mut eng, "SELECT id FROM u"), 1);
    assert_eq!(rows(&mut eng, "SELECT id FROM o"), 1);
}

#[test]
fn cascade_diamond_doesnt_double_delete() {
    // x ← y, x ← z, y ← w, z ← w; deleting x should remove
    // y/z/w each exactly once (w is reachable via both y and z).
    let mut eng = engine_with(&[
        "CREATE TABLE x (id INT NOT NULL)",
        "CREATE INDEX x_pk ON x (id)",
        "CREATE TABLE y (id INT NOT NULL, x_id INT NOT NULL, \
         FOREIGN KEY (x_id) REFERENCES x(id) ON DELETE CASCADE)",
        "CREATE INDEX y_pk ON y (id)",
        "CREATE TABLE z (id INT NOT NULL, x_id INT NOT NULL, \
         FOREIGN KEY (x_id) REFERENCES x(id) ON DELETE CASCADE)",
        "CREATE INDEX z_pk ON z (id)",
        "CREATE TABLE w (id INT NOT NULL, y_id INT NOT NULL, z_id INT NOT NULL, \
         FOREIGN KEY (y_id) REFERENCES y(id) ON DELETE CASCADE, \
         FOREIGN KEY (z_id) REFERENCES z(id) ON DELETE CASCADE)",
        "INSERT INTO x VALUES (1)",
        "INSERT INTO y VALUES (10, 1)",
        "INSERT INTO z VALUES (20, 1)",
        "INSERT INTO w VALUES (100, 10, 20)",
    ]);
    eng.execute("DELETE FROM x WHERE id = 1").unwrap();
    assert_eq!(rows(&mut eng, "SELECT id FROM x"), 0);
    assert_eq!(rows(&mut eng, "SELECT id FROM y"), 0);
    assert_eq!(rows(&mut eng, "SELECT id FROM z"), 0);
    assert_eq!(rows(&mut eng, "SELECT id FROM w"), 0);
}

#[test]
fn cascade_self_ref_subtree() {
    // org: 1 ← 2 ← 3; deleting 1 with ON DELETE CASCADE
    // should remove 2 and 3 too.
    let mut eng = engine_with(&[
        "CREATE TABLE org (id INT NOT NULL, parent_id INT, \
         FOREIGN KEY (parent_id) REFERENCES org(id) ON DELETE CASCADE)",
        "CREATE INDEX org_pk ON org (id)",
        "INSERT INTO org VALUES (1, NULL)",
        "INSERT INTO org VALUES (2, 1)",
        "INSERT INTO org VALUES (3, 2)",
    ]);
    eng.execute("DELETE FROM org WHERE id = 1").unwrap();
    assert_eq!(rows(&mut eng, "SELECT id FROM org"), 0);
}
