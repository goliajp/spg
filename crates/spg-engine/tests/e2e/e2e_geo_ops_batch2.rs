//! v7.39 (read01 utils/adt, geo_ops.c part 2) — the operator matrix:
//! `##` closest point, `#` intersections (box, line), polygon/circle
//! containment, polygon overlap, complex scaling of box/circle/path by
//! a point, the remaining `<->` distance pairs, and the `?|` / `?-`
//! alignment operators (binary points + prefix lseg). All values
//! byte-locked against PG18.

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
fn closest_point_operator() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT point '(0,0)' ## lseg '[(1,-1),(1,1)]', \
             point '(5,5)' ## box '((0,0),(2,2))', \
             point '(1,1)' ## box '((0,0),(2,2))'"
        ),
        vec!["(1,0)", "(2,2)", "(1,1)"]
    );
}

#[test]
fn intersection_operators() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT (lseg '[(0,0),(2,2)]' # lseg '[(0,2),(2,0)]'), \
             box '((0,0),(2,2))' # box '((1,1),(3,3))', \
             line '{1,-1,0}' # line '{1,1,0}'"
        ),
        vec!["(1,1)", "(2,2),(1,1)", "(0,0)"]
    );
    // Parallel lines / disjoint boxes intersect nowhere (NULL).
    assert_eq!(
        row_of(
            &mut e,
            "SELECT (line '{1,-1,0}' # line '{1,-1,5}') IS NULL, \
             (box '((0,0),(1,1))' # box '((2,2),(3,3))') IS NULL"
        ),
        vec!["true", "true"]
    );
}

#[test]
fn polygon_circle_containment_and_overlap() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT polygon '((0,0),(4,0),(4,4),(0,4))' @> polygon '((1,1),(2,1),(2,2))', \
             polygon '((0,0),(1,0),(1,1))' <@ polygon '((0,0),(4,0),(4,4),(0,4))', \
             circle '<(0,0),3>' @> circle '<(0,0),1>', \
             circle '<(0,0),1>' <@ circle '<(0,0),3>'"
        ),
        vec!["true", "true", "true", "true"]
    );
    assert_eq!(
        row_of(
            &mut e,
            "SELECT polygon '((0,0),(2,0),(2,2))' && polygon '((1,1),(3,1),(3,3))', \
             polygon '((0,0),(1,0),(1,1))' && polygon '((5,5),(6,5),(6,6))'"
        ),
        vec!["true", "false"]
    );
    // point <@ lseg / path (no @> spelling exists in PG).
    assert_eq!(
        row_of(
            &mut e,
            "SELECT point '(1,1)' <@ lseg '[(0,0),(2,2)]', point '(1,1)' <@ path '[(0,0),(2,2)]'"
        ),
        vec!["true", "true"]
    );
}

#[test]
fn complex_scaling() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT box '((0,0),(2,2))' * point '(2,0)', box '((0,0),(4,4))' / point '(2,0)', \
             circle '<(1,1),2>' * point '(2,0)', path '[(1,1),(2,2)]' * point '(0,1)'"
        ),
        vec!["(4,4),(0,0)", "(2,2),(0,0)", "<(2,2),4>", "[(-1,1),(-2,2)]"]
    );
}

#[test]
fn distance_matrix() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT point '(1,2)' <-> line '{1,-1,0}', \
             lseg '[(0,0),(1,1)]' <-> lseg '[(3,0),(3,3)]', \
             box '((0,0),(1,1))' <-> lseg '[(3,0),(3,3)]', \
             circle '<(0,0),1>' <-> circle '<(5,0),1>'"
        ),
        vec!["0.7071067811865476", "2", "2", "3"]
    );
    assert_eq!(
        row_of(
            &mut e,
            "SELECT point '(1,1)' <-> path '[(0,0),(2,0)]', \
             point '(0,3)' <-> polygon '((0,0),(2,0),(2,2),(0,2))', \
             point '(1,1)' <-> polygon '((0,0),(2,0),(2,2),(0,2))'"
        ),
        vec!["1", "1", "0"]
    );
}

#[test]
fn alignment_operators() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT point '(1,0)' ?| point '(1,5)', point '(1,0)' ?| point '(2,5)', \
             point '(0,1)' ?- point '(5,1)', point '(0,1)' ?- point '(5,2)'"
        ),
        vec!["true", "false", "true", "false"]
    );
    // Prefix forms test the lseg axis.
    assert_eq!(
        row_of(
            &mut e,
            "SELECT ?| lseg '[(1,0),(1,5)]', ?- lseg '[(0,1),(5,1)]', ?| lseg '[(0,0),(1,1)]'"
        ),
        vec!["true", "true", "false"]
    );
}
