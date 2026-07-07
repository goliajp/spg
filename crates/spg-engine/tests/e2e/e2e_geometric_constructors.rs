//! v7.38 (read01 sweep) — two-argument geometric constructors:
//! circle(point, radius), box(point, point), lseg(point, point). The
//! one-argument text spellings still route through the function-style
//! typecast. Oracle: live PG 18.4.

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
fn two_arg_geometric_constructors() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT (circle(point(1,1), 5))::text"), "<(1,1),5>");
    // PG normalises a box to (upper-right, lower-left).
    assert_eq!(text(&mut e, "SELECT (box(point(0,0), point(4,4)))::text"), "(4,4),(0,0)");
    assert_eq!(text(&mut e, "SELECT (lseg(point(0,0), point(3,4)))::text"), "[(0,0),(3,4)]");
    // Corners in any order normalise the same way.
    assert_eq!(text(&mut e, "SELECT (box(point(4,4), point(0,0)))::text"), "(4,4),(0,0)");
    // A constructed circle/box drives the containment operators.
    assert_eq!(text(&mut e, "SELECT circle(point(0,0), 5) @> point(3,4)"), "true");
    assert_eq!(text(&mut e, "SELECT box(point(0,0), point(10,10)) @> point(5,5)"), "true");
    // The one-argument text form still works via the typecast fallback.
    assert_eq!(text(&mut e, "SELECT circle('<(0,0),5>') @> point(3,4)"), "true");
}
