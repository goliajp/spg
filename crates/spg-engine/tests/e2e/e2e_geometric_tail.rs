//! v7.38 (read01, T21) — geometric tail: area-based box/circle equality, plus
//! slope / diagonal / bound_box / box(circle) / circle(box). Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => format!("{:?}", rows[0].values[0]),
        _ => panic!("rows"),
    }
}
fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            v => format!("{v:?}"),
        },
        _ => panic!("rows"),
    }
}

#[test]
fn geometric_tail() {
    let mut e = Engine::new();
    // Circle equality is by area (radius): same radius → equal even at a
    // different centre; different radius → not equal.
    assert_eq!(one(&mut e, "SELECT '<(0,0),5>'::circle = '<(9,9),5>'::circle"), "Bool(true)");
    assert_eq!(one(&mut e, "SELECT '<(0,0),5>'::circle = '<(1,1),6>'::circle"), "Bool(false)");
    // Box equality is by area too.
    assert_eq!(one(&mut e, "SELECT '(0,0),(1,1)'::box = '(5,5),(6,6)'::box"), "Bool(true)");
    assert_eq!(one(&mut e, "SELECT '(0,0),(2,2)'::box = '(0,0),(1,1)'::box"), "Bool(false)");
    // slope.
    assert_eq!(one(&mut e, "SELECT slope('(0,0)'::point, '(2,4)'::point)"), "Float(2.0)");
    assert_eq!(one(&mut e, "SELECT slope('(1,1)'::point, '(3,1)'::point)"), "Float(0.0)");
    assert_eq!(one(&mut e, "SELECT slope('(0,0)'::point, '(0,5)'::point)"), "Float(inf)");
    // diagonal / bound_box / box(circle) / circle(box).
    assert_eq!(text(&mut e, "SELECT (diagonal('(2,2),(0,0)'::box))::text"), "[(2,2),(0,0)]");
    assert_eq!(text(&mut e, "SELECT (bound_box('(0,0),(2,2)'::box, '(3,3),(1,1)'::box))::text"), "(3,3),(0,0)");
    assert_eq!(text(&mut e, "SELECT (box('<(1,1),2>'::circle))::text"),
        "(2.414213562373095,2.414213562373095),(-0.4142135623730949,-0.4142135623730949)");
    assert_eq!(text(&mut e, "SELECT (circle('(2,2),(0,0)'::box))::text"), "<(1,1),1.4142135623730951>");
}
