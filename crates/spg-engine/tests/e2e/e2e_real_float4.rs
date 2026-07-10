//! v7.38 (read01, T-float4) — REAL / float4 as a first-class 32-bit float:
//! f32 precision + rendering (float4out), pg_typeof, arithmetic (real op real
//! → real, mixed → double precision), comparison and aggregates. Oracle: PG18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            spg_storage::Value::Bool(b) => b.to_string(),
            v => format!("{v:?}"),
        },
        _ => panic!("expected rows"),
    }
}

#[test]
fn real_precision_and_rendering() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT ('3.14159265358979'::real)::text"),
        "3.1415927"
    );
    assert_eq!(text(&mut e, "SELECT (0.1::real)::text"), "0.1");
    assert_eq!(
        text(&mut e, "SELECT (12345678::real)::text"),
        "1.2345678e+07"
    ); // sci past exp 5
    assert_eq!(text(&mut e, "SELECT (100000::real)::text"), "100000");
    assert_eq!(text(&mut e, "SELECT (1e20::real)::text"), "1e+20");
}

#[test]
fn real_typeof_and_arithmetic() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT pg_typeof(1.5::real)"), "real");
    assert_eq!(text(&mut e, "SELECT pg_typeof(1.5::float4)"), "real");
    // real op real → real; real op {int, float8} → double precision (PG).
    assert_eq!(
        text(&mut e, "SELECT pg_typeof(1.5::real + 2.5::real)"),
        "real"
    );
    assert_eq!(text(&mut e, "SELECT (1.5::real + 2.5::real)::text"), "4");
    assert_eq!(text(&mut e, "SELECT (2.0::real * 3.0::real)::text"), "6");
    assert_eq!(
        text(&mut e, "SELECT pg_typeof(1.5::real + 1)"),
        "double precision"
    );
    assert_eq!(
        text(&mut e, "SELECT pg_typeof(1.5::real + 2.5::float8)"),
        "double precision"
    );
}

#[test]
fn real_comparison_and_aggregates() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT 1.5::real < 2.0::real"), "true");
    assert_eq!(text(&mut e, "SELECT 1.5::real = 1.5::real"), "true");
    let agg = "FROM (VALUES(1.5::real),(2.5::real),(0.5::real)) t(x)";
    assert_eq!(text(&mut e, &format!("SELECT (min(x))::text {agg}")), "0.5");
    assert_eq!(text(&mut e, &format!("SELECT (max(x))::text {agg}")), "2.5");
    assert_eq!(text(&mut e, &format!("SELECT (sum(x))::text {agg}")), "4.5");
}

#[test]
fn real_column_roundtrip() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT pg_typeof(x) FROM (SELECT 0.1::real AS x) t"),
        "real"
    );
    assert_eq!(
        text(&mut e, "SELECT (x)::text FROM (SELECT 0.1::real AS x) t"),
        "0.1"
    );
}
