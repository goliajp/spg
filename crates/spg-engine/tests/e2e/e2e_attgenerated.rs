//! v7.38 (read01 P6.41) — pg_attribute.attgenerated is 's' for a STORED
//! generated column (SPG stores generated columns as STORED), '' otherwise.
//! Oracle values from live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> Option<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.first().map(|r| match &r.values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            v => format!("{v:?}"),
        }),
        _ => None,
    }
}

#[test]
fn attgenerated_marks_stored_generated_columns() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE g(a int, b int GENERATED ALWAYS AS (a*2) STORED)")
        .unwrap();
    assert_eq!(
        one(
            &mut e,
            "SELECT attgenerated FROM pg_attribute WHERE attname='b'"
        ),
        Some("s".to_string())
    );
    // A plain column is '' — so filtering attgenerated='s' excludes it.
    assert_eq!(
        one(
            &mut e,
            "SELECT attname FROM pg_attribute WHERE attname='a' AND attgenerated='s'"
        ),
        None
    );
    // The generated value itself is still computed.
    e.execute("INSERT INTO g(a) VALUES (5)").unwrap();
    assert_eq!(one(&mut e, "SELECT b FROM g"), Some("Int(10)".to_string()));
}
