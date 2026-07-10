//! v7.38 (read01 sweep) — function-style typecast: PG treats `typename(expr)`
//! as shorthand for `expr::typename`. Works for primitives (int4/text/bool/
//! date), width/exotic types (float8/numeric), and geometric constructors
//! (circle/box) that previously errored with "unknown function". Oracle: PG18.4.

use spg_engine::{Engine, QueryResult};

fn cell(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => format!("{:?}", rows[0].values[0]),
        _ => panic!("expected rows"),
    }
}

#[test]
fn function_style_typecast_matches_cast() {
    let mut e = Engine::new();
    assert_eq!(cell(&mut e, "SELECT int4('5')"), "Int(5)");
    assert_eq!(cell(&mut e, "SELECT text(42)"), "Text(\"42\")");
    assert_eq!(cell(&mut e, "SELECT bool('t')"), "Bool(true)");
    assert_eq!(cell(&mut e, "SELECT float8('1.5')"), "Float(1.5)");
    assert_eq!(
        cell(&mut e, "SELECT (date('2024-01-15'))::text"),
        "Text(\"2024-01-15\")"
    );
    // Geometric constructors that used to error as "unknown function".
    assert_eq!(
        cell(&mut e, "SELECT circle('<(0,0),5>') @> point(3,4)"),
        "Bool(true)"
    );
    assert_eq!(
        cell(&mut e, "SELECT box('(0,0),(10,10)') @> point(5,5)"),
        "Bool(true)"
    );
    assert_eq!(
        cell(&mut e, "SELECT radius(circle('<(0,0),5>'))"),
        "Float(5.0)"
    );
    // A genuinely unknown function name still errors (not a type name).
    assert!(e.execute("SELECT nonexistent_fn(1)").is_err());
    // Two args → not a typecast even if the name is a type.
    assert!(e.execute("SELECT int4('5', '6')").is_err());
}
