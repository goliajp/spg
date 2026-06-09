//! v7.17.0 Phase 3.P0-56 — pg_catalog.pg_views / pg_matviews views.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn pg_views_lists_declared_view() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, label TEXT NOT NULL)")
        .unwrap();
    e.execute("CREATE VIEW v AS SELECT id, label FROM t")
        .unwrap();
    let r = rows(
        e.execute(
            "SELECT viewname, schemaname FROM pg_catalog.pg_views \
             WHERE viewname = 'v'",
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Text("v".into()));
    assert_eq!(r[0][1], Value::Text("public".into()));
}

#[test]
fn pg_views_definition_carries_body() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL)").unwrap();
    e.execute("CREATE VIEW v AS SELECT id FROM t").unwrap();
    let r = rows(
        e.execute("SELECT definition FROM pg_catalog.pg_views WHERE viewname = 'v'")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    let def = match &r[0][0] {
        Value::Text(s) => s.clone(),
        _ => panic!(),
    };
    assert!(def.to_uppercase().contains("SELECT"), "got: {def}");
}

#[test]
fn pg_matviews_is_empty_when_no_matviews() {
    // SPG has no materialised view surface yet; pg_matviews
    // intentionally surfaces the same shape as pg_views but the
    // engine has none to enumerate so the table is empty.
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT COUNT(*) FROM pg_catalog.pg_matviews")
            .unwrap(),
    );
    assert_eq!(r[0][0], Value::BigInt(0));
}
