//! v7.37.17 (17.6 siblings) — enum introspection + reg*_to_text
//! object-identifier converters.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
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
fn enum_introspection_returns_null() {
    let mut e = Engine::new();
    for f in &[
        "enum_first('open'::text)",
        "enum_last('open'::text)",
        "enum_range('open'::text)",
        "enum_range_between('open'::text, 'closed'::text)",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}

#[test]
fn regclass_to_text_roundtrips_text() {
    let mut e = Engine::new();
    assert_eq!(
        text(&first(&mut e, "SELECT regclass_to_text('public.users')")),
        "public.users"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT regtype_to_text('integer')")),
        "integer"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT regrole_to_text('admin')")),
        "admin"
    );
}

#[test]
fn regclass_to_text_int_input() {
    let mut e = Engine::new();
    assert_eq!(
        text(&first(&mut e, "SELECT regclass_to_text(42)")),
        "42"
    );
}

#[test]
fn reg_to_text_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT regclass_to_text(NULL::text)"),
        spg_storage::Value::Null
    ));
}
