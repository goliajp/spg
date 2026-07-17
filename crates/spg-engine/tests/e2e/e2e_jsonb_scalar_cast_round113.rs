//! v7.39 (read01 round 113) — `jsonb` → scalar casts.
//!
//! `('5'::jsonb)::int` errored ("cannot cast Json to int"): SPG had no path
//! from a jsonb value to a numeric/bool target. PG decodes the underlying JSON
//! scalar first — a JSON number → numeric (int targets round half-away), a JSON
//! true/false → bool, `null` → SQL NULL; a JSON string / array / object (and a
//! number→bool or bool→number mismatch) errors "cannot cast jsonb <kind> to
//! type <target>". Now covers int / bigint / smallint / numeric / real /
//! float8 / bool. Fixing the numeric-target path also revealed (and closed) a
//! doubled "eval: type mismatch:" class prefix in the generic Named-cast error
//! wrap. Locked byte-identical against PG 18.4.
//!
//! (SPG collapses json and jsonb into one runtime value, so `('5'::json)::int`
//! also works here — a documented SPG-lenient divergence; PG rejects it.)

use spg_engine::{Engine, QueryResult};

fn scalar(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => rows[0].values[0].clone(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn render(e: &mut Engine, sql: &str) -> String {
    match scalar(e, sql) {
        spg_storage::Value::Null => "NULL".to_string(),
        v => spg_engine::eval::value_to_text(&v),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(x) => format!("{x}"),
        Ok(ok) => panic!("{sql}: expected error, got {ok:?}"),
    }
}

#[test]
fn jsonb_number_to_integer_targets() {
    let mut e = Engine::new();
    // Integer targets round via NUMERIC (half-away), like `2.5::numeric::int`.
    assert!(matches!(
        scalar(&mut e, "SELECT ('5'::jsonb)::int"),
        spg_storage::Value::Int(5)
    ));
    assert!(matches!(
        scalar(&mut e, "SELECT ('2.5'::jsonb)::int"),
        spg_storage::Value::Int(3)
    ));
    assert!(matches!(
        scalar(&mut e, "SELECT ('-2.5'::jsonb)::int"),
        spg_storage::Value::Int(-3)
    ));
    assert!(matches!(
        scalar(&mut e, "SELECT ('1.5'::jsonb)::int"),
        spg_storage::Value::Int(2)
    ));
    assert!(matches!(
        scalar(&mut e, "SELECT ('5'::jsonb)::bigint"),
        spg_storage::Value::BigInt(5)
    ));
    assert!(matches!(
        scalar(&mut e, "SELECT ('5'::jsonb)::smallint"),
        spg_storage::Value::SmallInt(5)
    ));
}

#[test]
fn jsonb_number_to_fractional_targets() {
    let mut e = Engine::new();
    assert_eq!(render(&mut e, "SELECT ('5.5'::jsonb)::numeric"), "5.5");
    assert_eq!(render(&mut e, "SELECT ('5.5'::jsonb)::float8"), "5.5");
    assert_eq!(render(&mut e, "SELECT ('5.5'::jsonb)::real"), "5.5");
}

#[test]
fn jsonb_bool_and_null() {
    let mut e = Engine::new();
    assert!(matches!(
        scalar(&mut e, "SELECT ('true'::jsonb)::bool"),
        spg_storage::Value::Bool(true)
    ));
    assert!(matches!(
        scalar(&mut e, "SELECT ('false'::jsonb)::bool"),
        spg_storage::Value::Bool(false)
    ));
    // JSON null → SQL NULL (not the string "null").
    assert!(matches!(
        scalar(&mut e, "SELECT ('null'::jsonb)::int"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        scalar(&mut e, "SELECT ('null'::jsonb)::bool"),
        spg_storage::Value::Null
    ));
}

#[test]
fn jsonb_non_castable_kinds_error() {
    let mut e = Engine::new();
    // A JSON string / boolean / array / object is not castable to a number.
    assert!(
        err(&mut e, "SELECT ('\"7\"'::jsonb)::int")
            .contains("cannot cast jsonb string to type integer")
    );
    assert!(
        err(&mut e, "SELECT ('true'::jsonb)::int")
            .contains("cannot cast jsonb boolean to type integer")
    );
    assert!(
        err(&mut e, "SELECT ('[1,2]'::jsonb)::int")
            .contains("cannot cast jsonb array to type integer")
    );
    assert!(
        err(&mut e, "SELECT ('{\"a\":1}'::jsonb)::int")
            .contains("cannot cast jsonb object to type integer")
    );
    // A number is not castable to bool, and a string is not castable to numeric.
    assert!(
        err(&mut e, "SELECT ('5'::jsonb)::bool")
            .contains("cannot cast jsonb numeric to type boolean")
    );
    assert!(
        err(&mut e, "SELECT ('\"7\"'::jsonb)::numeric")
            .contains("cannot cast jsonb string to type numeric")
    );
    // The numeric-target message carries no doubled class prefix.
    assert!(
        !err(&mut e, "SELECT ('\"7\"'::jsonb)::numeric").contains("mismatch: eval: type mismatch")
    );
    // Out-of-range integer errors (not saturates).
    assert!(err(&mut e, "SELECT ('1e10'::jsonb)::int").contains("integer out of range"));
}
