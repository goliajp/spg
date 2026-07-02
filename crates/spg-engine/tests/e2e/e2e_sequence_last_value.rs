//! v7.37.17 (17.6 siblings) — pg_sequence_last_value upgraded from
//! NULL stub to real catalog read.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn last_value_tracks_nextval() {
    let mut e = Engine::new();
    e.execute("CREATE SEQUENCE slv").unwrap();
    // Never advanced → NULL (PG semantics for is_called=false).
    assert!(matches!(
        first(&mut e, "SELECT pg_sequence_last_value('slv')"),
        spg_storage::Value::Null
    ));
    e.execute("SELECT nextval('slv')").unwrap();
    e.execute("SELECT nextval('slv')").unwrap();
    assert!(matches!(
        first(&mut e, "SELECT pg_sequence_last_value('slv')"),
        spg_storage::Value::BigInt(2)
    ));
    // 'public.' qualification accepted.
    assert!(matches!(
        first(&mut e, "SELECT pg_sequence_last_value('public.slv')"),
        spg_storage::Value::BigInt(2)
    ));
}

#[test]
fn missing_sequence_is_null() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT pg_sequence_last_value('nope')"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT pg_sequence_last_value(NULL::text)"),
        spg_storage::Value::Null
    ));
}
