//! Cast-target batch 2 — ::time, ::float4/float8, ::oid, and
//! explicit varchar(n) truncation (PG semantics).

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn varchar_explicit_cast_truncates() {
    let mut e = Engine::new();
    // PG: SELECT 'hello'::varchar(3) → 'hel' (only column
    // assignment errors on overflow).
    assert!(matches!(
        one(&mut e, "SELECT 'hello'::varchar(3)"),
        spg_storage::Value::Text(ref s) if s == "hel"
    ));
    assert!(matches!(
        one(&mut e, "SELECT 'ok'::varchar(5)"),
        spg_storage::Value::Text(ref s) if s == "ok"
    ));
}

#[test]
fn float_and_oid_spellings() {
    let mut e = Engine::new();
    assert!(matches!(
        one(&mut e, "SELECT 2::float4"),
        spg_storage::Value::Float(f) if (f - 2.0).abs() < 1e-9
    ));
    assert!(matches!(
        one(&mut e, "SELECT 2::float8"),
        spg_storage::Value::Float(f) if (f - 2.0).abs() < 1e-9
    ));
    assert!(matches!(
        one(&mut e, "SELECT '2.5'::real"),
        spg_storage::Value::Float(f) if (f - 2.5).abs() < 1e-9
    ));
    assert!(matches!(
        one(&mut e, "SELECT 42::oid"),
        spg_storage::Value::BigInt(42) | spg_storage::Value::Int(42)
    ));
}

#[test]
fn time_cast_parses() {
    let mut e = Engine::new();
    let v = one(&mut e, "SELECT '12:34:56'::time");
    assert!(
        matches!(v, spg_storage::Value::Time(us)
            if us == ((12 * 3600 + 34 * 60 + 56) as i64) * 1_000_000),
        "unexpected: {v:?}"
    );
}
