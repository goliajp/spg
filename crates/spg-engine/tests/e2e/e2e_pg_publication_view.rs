//! v7.37.21 (21.13-b) — `pg_catalog.pg_publication` view. Lists
//! every CREATE PUBLICATION the engine holds. Logical-replication
//! subscribers query this at handshake to validate the publication
//! exists.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<Value<'static>>> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.into_iter().map(|r| r.values).collect()
}

#[test]
fn pg_publication_has_pg_canonical_columns() {
    let mut e = Engine::new();
    let r = e.execute("SELECT * FROM pg_catalog.pg_publication").unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!("Rows");
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    for must in [
        "oid",
        "pubname",
        "pubowner",
        "puballtables",
        "pubinsert",
        "pubupdate",
        "pubdelete",
        "pubtruncate",
        "pubviaroot",
    ] {
        assert!(
            names.contains(&must),
            "pg_publication missing column {must}, got {names:?}"
        );
    }
}

#[test]
fn pg_publication_lists_created_publications() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT)").unwrap();
    e.execute("CREATE PUBLICATION p_all FOR ALL TABLES").unwrap();
    e.execute("CREATE PUBLICATION p_t FOR TABLE t").unwrap();
    let rs = rows(&mut e, "SELECT * FROM pg_catalog.pg_publication");
    let names: Vec<String> = rs
        .iter()
        .filter_map(|r| {
            if let Value::Text(s) = &r[1] {
                Some(s.to_string())
            } else {
                None
            }
        })
        .collect();
    assert!(names.contains(&"p_all".to_string()));
    assert!(names.contains(&"p_t".to_string()));
    // puballtables at position 3 should be true for p_all, false for p_t.
    let p_all = rs
        .iter()
        .find(|r| matches!(&r[1], Value::Text(s) if s.as_ref() == "p_all"))
        .unwrap();
    let p_t = rs
        .iter()
        .find(|r| matches!(&r[1], Value::Text(s) if s.as_ref() == "p_t"))
        .unwrap();
    assert!(matches!(p_all[3], Value::Bool(true)));
    assert!(matches!(p_t[3], Value::Bool(false)));
}

#[test]
fn pg_publication_empty_when_no_publications() {
    let mut e = Engine::new();
    let rs = rows(&mut e, "SELECT * FROM pg_catalog.pg_publication");
    assert!(rs.is_empty(), "got {rs:?}");
}
