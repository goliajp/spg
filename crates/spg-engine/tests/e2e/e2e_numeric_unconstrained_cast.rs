//! v7.38 (read01 sweep) — casting an existing NUMERIC value through the
//! unconstrained `::numeric` sentinel (precision 0, scale 0) must keep the
//! value's natural scale, not round it to an integer. This matches the
//! Float→Numeric and Text→Numeric cast arms, and PG 18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            v => format!("{v:?}"),
        },
        _ => panic!("expected rows"),
    }
}

#[test]
fn unconstrained_numeric_cast_keeps_scale() {
    let mut e = Engine::new();
    // A declared numeric(p,s) produces a scaled NUMERIC; re-casting it through
    // the bare `::numeric` must NOT round it.
    assert_eq!(
        text(&mut e, "SELECT (2.5::numeric(3,1)::numeric)::text"),
        "2.5"
    );
    assert_eq!(
        text(&mut e, "SELECT (3.14::numeric(5,2)::numeric)::text"),
        "3.14"
    );
    assert_eq!(
        text(&mut e, "SELECT (123.456::numeric(10,3)::numeric)::text"),
        "123.456"
    );
    // A numeric column re-cast to unconstrained numeric keeps its stored scale.
    e.execute("CREATE TABLE nc (v numeric(6,2))").unwrap();
    e.execute("INSERT INTO nc VALUES (12.34)").unwrap();
    assert_eq!(text(&mut e, "SELECT (v::numeric)::text FROM nc"), "12.34");
    // A declared numeric(p,s) target still rescales (rounds half away from zero).
    assert_eq!(
        text(&mut e, "SELECT (2.567::numeric(10,4)::numeric(4,2))::text"),
        "2.57"
    );
}
