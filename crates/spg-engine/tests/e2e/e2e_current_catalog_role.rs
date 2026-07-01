//! v7.37.17 (17.6 siblings) — SQL:2003 CURRENT_CATALOG / CURRENT_ROLE
//! synonyms for CURRENT_DATABASE / CURRENT_USER.

use spg_engine::{Engine, QueryResult};

fn first_text(e: &mut Engine, sql: &str) -> String {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("Rows");
    };
    match &rows[0].values[0] {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn current_catalog_matches_current_database() {
    let mut e = Engine::new();
    let cat = first_text(&mut e, "SELECT current_catalog");
    let db = first_text(&mut e, "SELECT current_database()");
    assert_eq!(cat, db);
}

#[test]
fn current_role_matches_current_user() {
    let mut e = Engine::new();
    let role = first_text(&mut e, "SELECT current_role");
    let user = first_text(&mut e, "SELECT current_user");
    assert_eq!(role, user);
}

#[test]
fn current_catalog_role_composable_in_where() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT, db TEXT, role TEXT)").unwrap();
    e.execute("INSERT INTO t VALUES (1, 'spg', 'admin')").unwrap();
    let r = e
        .execute("SELECT id FROM t WHERE db = current_catalog AND role = current_role")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!("Rows");
    };
    assert_eq!(rows.len(), 1);
}
