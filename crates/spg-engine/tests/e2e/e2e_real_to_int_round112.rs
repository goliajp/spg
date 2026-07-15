//! v7.39 (read01 round 112) — `real` (float4) → integer casts.
//!
//! `(1.9::real)::int` errored ("cannot cast Real to int"): the numeric-to-int
//! cast handlers only had a `Value::Float` (float8) arm, never a `Value::Real`
//! (float4) one — the sibling of round 110's real → numeric gap. real now
//! rounds half-to-even and range-checks exactly like float8, across int /
//! bigint / smallint. Locked byte-identical against PG 18.4.

use spg_engine::{Engine, QueryResult};

fn scalar(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => rows[0].values[0].clone(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn real_to_int_rounds_half_even() {
    let mut e = Engine::new();
    // Round half-to-even, matching PG: 2.5 → 2, 3.5 → 4, -2.5 → -2.
    assert!(matches!(scalar(&mut e, "SELECT (1.9::real)::int"), spg_storage::Value::Int(2)));
    assert!(matches!(scalar(&mut e, "SELECT (2.5::real)::int"), spg_storage::Value::Int(2)));
    assert!(matches!(scalar(&mut e, "SELECT (3.5::real)::int"), spg_storage::Value::Int(4)));
    assert!(matches!(scalar(&mut e, "SELECT (-2.5::real)::int"), spg_storage::Value::Int(-2)));
    assert!(matches!(scalar(&mut e, "SELECT (0.4::real)::int"), spg_storage::Value::Int(0)));
    assert!(matches!(scalar(&mut e, "SELECT (0.6::real)::int"), spg_storage::Value::Int(1)));
}

#[test]
fn real_to_bigint_and_smallint() {
    let mut e = Engine::new();
    assert!(matches!(scalar(&mut e, "SELECT (100::real)::bigint"), spg_storage::Value::BigInt(100)));
    assert!(matches!(scalar(&mut e, "SELECT (100::real)::smallint"), spg_storage::Value::SmallInt(100)));
    assert!(matches!(scalar(&mut e, "SELECT (-2.5::real)::smallint"), spg_storage::Value::SmallInt(-2)));
    assert!(matches!(scalar(&mut e, "SELECT (32767.4::real)::smallint"), spg_storage::Value::SmallInt(32767)));
}

#[test]
fn real_to_int_out_of_range_errors() {
    let mut e = Engine::new();
    // PG errors (not saturates) when the rounded value doesn't fit.
    assert!(e.execute("SELECT (32768::real)::smallint").is_err());
    assert!(e.execute("SELECT (3e9::real)::int").is_err());
}
