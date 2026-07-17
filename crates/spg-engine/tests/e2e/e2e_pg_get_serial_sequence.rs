//! v7.37.17 (17.6 siblings) — pg_get_serial_sequence.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn pg_get_serial_sequence_bare_table() {
    // v7.39 (read01 ruleutils.c) — only serial/identity columns map to a
    // sequence; a plain column is NULL and a missing relation errors (PG).
    let mut e = Engine::new();
    e.execute("CREATE TABLE users (id SERIAL, note TEXT)")
        .unwrap();
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT pg_get_serial_sequence('users', 'id')"
        )),
        "public.users_id_seq"
    );
    assert!(matches!(
        first(&mut e, "SELECT pg_get_serial_sequence('users', 'note')"),
        spg_storage::Value::Null
    ));
    let err = e
        .execute("SELECT pg_get_serial_sequence('nope', 'id')")
        .unwrap_err();
    assert!(
        format!("{err}").contains("relation \"nope\" does not exist"),
        "{err}"
    );
}

#[test]
fn pg_get_serial_sequence_qualified_table() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE orders (order_id SERIAL)").unwrap();
    // Strips the leading schema for the sequence-name computation.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT pg_get_serial_sequence('public.orders', 'order_id')"
        )),
        "public.orders_order_id_seq"
    );
}

#[test]
fn pg_get_serial_sequence_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT pg_get_serial_sequence(NULL::text, 'id')"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT pg_get_serial_sequence('users', NULL::text)"),
        spg_storage::Value::Null
    ));
}
