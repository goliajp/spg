//! v7.17.0 Phase 3.P0-65 — mysql.user / mysql.db.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value<'static>>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn mysql_user_lists_root() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT user, host FROM mysql.user WHERE user = 'root'")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::text("root"));
    assert_eq!(r[0][1], Value::text("localhost"));
}

#[test]
fn mysql_db_lists_canonical_grants() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT user, db FROM mysql.db WHERE db = 'postgres'")
            .unwrap(),
    );
    assert!(!r.is_empty());
    assert_eq!(r[0][1], Value::text("postgres"));
}
