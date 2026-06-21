//! v7.9.0/v7.9.1 — JSONB column type. Storage shape matches
//! JSON; PG-wire OID differs. Tests cover DDL accept, INSERT
//! round-trip, SELECT through string, catalog snapshot survival.

use spg_engine::{Engine, QueryResult};
use spg_storage::{DataType, Value};

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

fn select(eng: &mut Engine, sql: &str) -> Vec<Vec<Value<'static>>> {
    match eng.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected Rows"),
    }
}

#[test]
fn ddl_accepts_jsonb_keyword() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t (id INT NOT NULL, payload JSONB)")
        .unwrap();
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let schema = cat.get("t").unwrap().schema();
    assert!(matches!(schema.columns[1].ty, DataType::Jsonb));
}

#[test]
fn insert_and_select_jsonb_round_trips_verbatim() {
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, payload JSONB)",
        "INSERT INTO t VALUES (1, '{\"k\":\"v\",\"n\":42}')",
    ]);
    let rows = select(&mut eng, "SELECT payload FROM t");
    assert_eq!(rows.len(), 1);
    let Value::Json(s) = &rows[0][0] else {
        panic!("expected Value::Json, got {:?}", rows[0][0])
    };
    assert_eq!(s, r#"{"k":"v","n":42}"#);
}

#[test]
fn json_and_jsonb_can_coexist_in_same_table() {
    let mut eng = engine_with(&[
        "CREATE TABLE mix (id INT NOT NULL, a JSON, b JSONB)",
        "INSERT INTO mix VALUES (1, '\"json\"', '\"jsonb\"')",
    ]);
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let cols = &cat.get("mix").unwrap().schema().columns;
    assert!(matches!(cols[1].ty, DataType::Json));
    assert!(matches!(cols[2].ty, DataType::Jsonb));
}

#[test]
fn jsonb_column_survives_catalog_round_trip() {
    let mut eng = engine_with(&[
        "CREATE TABLE webhook (id INT NOT NULL, payload JSONB)",
        "INSERT INTO webhook VALUES (1, '{\"event\":\"delivered\",\"ts\":\"now\"}')",
        "INSERT INTO webhook VALUES (2, '{\"event\":\"bounced\"}')",
    ]);
    let bytes = eng.snapshot();
    let cat = spg_storage::Catalog::deserialize(&bytes).unwrap();
    let mut eng2 = Engine::restore(cat);
    let rows = select(&mut eng2, "SELECT payload FROM webhook");
    assert_eq!(rows.len(), 2);
}

#[test]
fn jsonb_text_cross_compat_works_both_directions() {
    // SQL-level: TEXT literal can land in JSONB column;
    // JSONB column reads as Value::Json which serialises as text.
    let mut eng = engine_with(&[
        "CREATE TABLE t (id INT NOT NULL, raw TEXT, parsed JSONB)",
        "INSERT INTO t VALUES (1, '{}', '{}')",
    ]);
    let rows = select(&mut eng, "SELECT raw, parsed FROM t");
    assert_eq!(rows.len(), 1);
    // Both should display the same content; only the stored
    // Value variant differs (Text vs Json).
    let Value::Text(t) = &rows[0][0] else {
        panic!("col 0 should be Text")
    };
    let Value::Json(j) = &rows[0][1] else {
        panic!("col 1 should be Json")
    };
    assert_eq!(t, "{}");
    assert_eq!(j, "{}");
}
