//! v7.37.5 ε — PG geometry scalar smoke tests.
//!
//! Pins one round-trip per geometric type:
//!   point   `(x,y)`                      OID 600 / tag 50 / 16 B
//!   lseg    `[(x1,y1),(x2,y2)]`          OID 601 / tag 51 / 32 B
//!   path    `[...]` open / `(...)` closed OID 602 / tag 52 / var
//!   box     `(ux,uy),(lx,ly)`            OID 603 / tag 53 / 32 B
//!   polygon `((x,y),...)`                OID 604 / tag 54 / var
//!   line    `{a,b,c}`                    OID 628 / tag 55 / 24 B
//!   circle  `<(x,y),r>`                  OID 718 / tag 56 / 24 B
//!
//! Each test pins DDL accept + INSERT text literal → SELECT
//! round-trip + (where applicable) the Text↔geometry inverse
//! coerce.

use spg_engine::{Engine, QueryResult};
use spg_storage::{DataType, Point2D, Value};

fn rows(r: QueryResult) -> Vec<Vec<Value<'static>>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected Rows"),
    }
}

fn col_type(e: &mut Engine, sql: &str) -> DataType {
    let r = e.execute(sql).unwrap();
    let QueryResult::Rows { columns, .. } = r else {
        panic!()
    };
    columns[0].ty
}

fn point(x: f64, y: f64) -> Point2D {
    Point2D { x, y }
}

#[test]
fn point_round_trip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, p POINT NOT NULL)")
        .unwrap();
    assert_eq!(col_type(&mut e, "SELECT p FROM t"), DataType::Point);
    e.execute("INSERT INTO t VALUES (1, '(1.5,-2.25)'::point)")
        .unwrap();
    let r = rows(e.execute("SELECT p FROM t").unwrap());
    assert_eq!(r[0][0], Value::Point(point(1.5, -2.25)));
}

#[test]
fn lseg_round_trip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, seg LSEG NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, '[(0,0),(10,5)]'::lseg)")
        .unwrap();
    let r = rows(e.execute("SELECT seg FROM t").unwrap());
    assert_eq!(r[0][0], Value::Lseg(point(0.0, 0.0), point(10.0, 5.0)));
}

#[test]
fn box_round_trip_with_normalization() {
    // PG normalises the corner order: input `(0,0),(5,5)` lands
    // as upper-right `(5,5)` + lower-left `(0,0)`.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, b BOX NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, '(0,0),(5,5)'::box)")
        .unwrap();
    let r = rows(e.execute("SELECT b FROM t").unwrap());
    assert_eq!(r[0][0], Value::PgBox(point(5.0, 5.0), point(0.0, 0.0)));
}

#[test]
fn line_round_trip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, l LINE NOT NULL)")
        .unwrap();
    // Line `2x + 3y - 6 = 0`.
    e.execute("INSERT INTO t VALUES (1, '{2,3,-6}'::line)")
        .unwrap();
    let r = rows(e.execute("SELECT l FROM t").unwrap());
    assert_eq!(
        r[0][0],
        Value::Line {
            a: 2.0,
            b: 3.0,
            c: -6.0
        }
    );
}

#[test]
fn circle_round_trip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, c CIRCLE NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, '<(1,2),3.5>'::circle)")
        .unwrap();
    let r = rows(e.execute("SELECT c FROM t").unwrap());
    assert_eq!(
        r[0][0],
        Value::Circle {
            center: point(1.0, 2.0),
            radius: 3.5,
        }
    );
}

#[test]
fn path_open_round_trip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, p PATH NOT NULL)")
        .unwrap();
    // `[...]` brackets pin open-form.
    e.execute("INSERT INTO t VALUES (1, '[(0,0),(1,1),(2,4)]'::path)")
        .unwrap();
    let r = rows(e.execute("SELECT p FROM t").unwrap());
    assert_eq!(
        r[0][0],
        Value::Path {
            points: vec![point(0.0, 0.0), point(1.0, 1.0), point(2.0, 4.0)],
            closed: false,
        }
    );
}

#[test]
fn path_closed_round_trip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, p PATH NOT NULL)")
        .unwrap();
    // `(...)` parens pin closed-form.
    e.execute("INSERT INTO t VALUES (1, '((0,0),(3,0),(3,3),(0,3))'::path)")
        .unwrap();
    let r = rows(e.execute("SELECT p FROM t").unwrap());
    let Value::Path { points, closed } = &r[0][0] else {
        panic!()
    };
    assert!(*closed);
    assert_eq!(points.len(), 4);
}

#[test]
fn polygon_round_trip() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, p POLYGON NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, '((0,0),(4,0),(4,3),(0,3))'::polygon)")
        .unwrap();
    let r = rows(e.execute("SELECT p FROM t").unwrap());
    assert_eq!(
        r[0][0],
        Value::Polygon(vec![
            point(0.0, 0.0),
            point(4.0, 0.0),
            point(4.0, 3.0),
            point(0.0, 3.0),
        ])
    );
}

#[test]
fn point_cast_to_text_renders_canonical() {
    let mut e = Engine::new();
    let r = rows(e.execute("SELECT '(7,-3)'::point::text").unwrap());
    let Value::Text(s) = &r[0][0] else {
        panic!("got {:?}", r[0][0]);
    };
    assert_eq!(s, "(7,-3)");
}

#[test]
fn nullable_geometry_column_accepts_null() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, p POINT)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, NULL), (2, '(0,0)'::point)")
        .unwrap();
    let r = rows(e.execute("SELECT p FROM t ORDER BY id").unwrap());
    assert_eq!(r[0][0], Value::Null);
    assert_eq!(r[1][0], Value::Point(point(0.0, 0.0)));
}
