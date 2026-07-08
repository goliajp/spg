//! Unconstrained ::numeric preserves the source scale instead of
//! truncating to 0 decimals (3.14::numeric stays 3.14, not 3).

use spg_engine::{Engine, QueryResult};

fn numeric(e: &mut Engine, sql: &str) -> (i128, u8) {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    match rows[0].values[0] {
        spg_storage::Value::Numeric { scaled, scale , .. } => (scaled, scale),
        ref other => panic!("{sql}: expected Numeric, got {other:?}"),
    }
}

#[test]
fn unconstrained_numeric_keeps_scale() {
    let mut e = Engine::new();
    // Float literal → unconstrained numeric keeps 3.14.
    assert_eq!(numeric(&mut e, "SELECT 3.14::numeric"), (314, 2));
    // Text literal too.
    assert_eq!(numeric(&mut e, "SELECT '3.14'::numeric"), (314, 2));
    assert_eq!(numeric(&mut e, "SELECT '0.0001'::numeric"), (1, 4));
    // Integer stays scale 0.
    assert_eq!(numeric(&mut e, "SELECT 42::numeric"), (42, 0));
    // decimal spelling behaves the same.
    assert_eq!(numeric(&mut e, "SELECT 2.5::decimal"), (25, 1));
}

#[test]
fn constrained_numeric_still_rescales() {
    let mut e = Engine::new();
    // numeric(p,s) still rounds/rescales to the declared scale.
    assert_eq!(numeric(&mut e, "SELECT 3.14159::numeric(10,2)"), (314, 2));
    assert_eq!(numeric(&mut e, "SELECT 3::numeric(10,2)"), (300, 2));
}
