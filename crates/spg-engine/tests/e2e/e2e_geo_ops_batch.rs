//! v7.39 (read01 utils/adt, geo_ops.c part 1) — geometric gaps found by
//! differential against PG18: path/polygon functions (area / isclosed /
//! isopen / npoints / pclose / popen), slope, the two-point line input
//! form, box→polygon conversion, the `?||` / `?-|` / `~=` operators and
//! the prefix `@@` (center-of) operator. All values byte-locked vs PG18.

use spg_engine::{Engine, QueryResult};

fn row_of(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows[0]
            .values
            .iter()
            .map(spg_engine::eval::value_to_text)
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn path_functions() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT area(path '((0,0),(4,0),(4,4),(0,4))'), \
             isclosed(path '((0,0),(1,1))'), isopen(path '[(0,0),(1,1)]'), \
             npoints(path '[(0,0),(1,1),(2,2)]')"
        ),
        vec!["16", "true", "true", "3"]
    );
    // Open path has no area (NULL); pclose/popen flip the form.
    assert_eq!(
        row_of(
            &mut e,
            "SELECT area(path '[(0,0),(4,0),(4,4)]') IS NULL, \
             pclose(path '[(0,0),(1,1)]'), popen(path '((0,0),(1,1))')"
        ),
        vec!["true", "((0,0),(1,1))", "[(0,0),(1,1)]"]
    );
}

#[test]
fn slope_and_line_two_point_form() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT slope(point '(0,0)', point '(2,4)'), \
             slope(point '(1,0)', point '(1,5)'), \
             slope(point '(0,2)', point '(5,2)')"
        ),
        vec!["2", "Infinity", "0"]
    );
    // Two-point line input builds Ax+By+C=0 from the slope.
    assert_eq!(
        row_of(
            &mut e,
            "SELECT line '((0,0),(1,1))', line '((2,0),(2,5))', line '((0,3),(5,3))'"
        ),
        vec!["{1,-1,0}", "{-1,0,2}", "{0,-1,3}"]
    );
}

#[test]
fn box_to_polygon_conversion() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(&mut e, "SELECT polygon(box '((0,0),(2,2))')"),
        vec!["((0,0),(0,2),(2,2),(2,0))"]
    );
}

#[test]
fn parallel_perpendicular_sameas_operators() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT lseg '[(0,0),(1,1)]' ?|| lseg '[(2,2),(3,3)]', \
             lseg '[(0,0),(1,1)]' ?-| lseg '[(0,1),(1,0)]', \
             lseg '[(0,0),(1,1)]' ?|| lseg '[(0,0),(1,2)]'"
        ),
        vec!["true", "true", "false"]
    );
    assert_eq!(
        row_of(
            &mut e,
            "SELECT point '(1,2)' ~= point '(1,2)', \
             box '((0,0),(1,1))' ~= box '((1,1),(0,0))', \
             point '(1,2)' ~= point '(1,3)'"
        ),
        vec!["true", "true", "false"]
    );
}

#[test]
fn prefix_center_operator() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT @@ box '((0,0),(2,4))', @@ circle '<(3,3),1>', @@ lseg '[(0,0),(2,2)]'"
        ),
        vec!["(1,2)", "(3,3)", "(1,1)"]
    );
}
