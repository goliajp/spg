//! v7.37.17 (17.6 siblings) — range constructor functions
//! (int4range / numrange / daterange / tsrange etc.). Until now
//! ranges only entered SPG through '::int4range' text casts.

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
fn int4range_default_and_explicit_bounds() {
    let mut e = Engine::new();
    // Default bounds are '[)' — matches the text-cast canonical form.
    assert_eq!(
        first(&mut e, "SELECT int4range(1, 5)"),
        first(&mut e, "SELECT '[1,5)'::int4range")
    );
    assert_eq!(
        first(&mut e, "SELECT int4range(1, 5, '(]')"),
        first(&mut e, "SELECT '(1,5]'::int4range")
    );
    // Composes with the #256 bound predicates.
    assert!(matches!(
        first(&mut e, "SELECT lower_inc(int4range(1, 5))"),
        spg_storage::Value::Bool(true)
    ));
    assert!(matches!(
        first(&mut e, "SELECT upper_inc(int4range(1, 5))"),
        spg_storage::Value::Bool(false)
    ));
}

#[test]
fn null_bounds_are_unbounded() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT lower_inf(int8range(NULL, 10))"),
        spg_storage::Value::Bool(true)
    ));
    assert!(matches!(
        first(&mut e, "SELECT upper_inf(int8range(1, NULL))"),
        spg_storage::Value::Bool(true)
    ));
}

#[test]
fn equal_bounds_collapse_to_empty() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT isempty(int4range(5, 5))"),
        spg_storage::Value::Bool(true)
    ));
    // '[]' keeps the single point.
    assert!(matches!(
        first(&mut e, "SELECT isempty(int4range(5, 5, '[]'))"),
        spg_storage::Value::Bool(false)
    ));
}

#[test]
fn numrange_and_daterange() {
    let mut e = Engine::new();
    // PG doc shape: numrange(1.1, 2.2).
    let rendered = text(&first(&mut e, "SELECT numrange(1.1, 2.2)::text"));
    assert!(
        rendered.contains("1.1") && rendered.contains("2.2"),
        "unexpected render: {rendered}"
    );
    assert_eq!(
        first(
            &mut e,
            "SELECT daterange(DATE '2003-01-01', DATE '2003-02-01')"
        ),
        first(&mut e, "SELECT '[2003-01-01,2003-02-01)'::daterange")
    );
}
