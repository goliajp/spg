//! v7.37.17 (17.6 siblings) — multirange constructor functions
//! (int4multirange etc.), variadic over ranges.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn multirange_from_ranges() {
    let mut e = Engine::new();
    let got = text(&first(
        &mut e,
        "SELECT int4multirange(int4range(1, 3), int4range(5, 8))::text",
    ));
    assert_eq!(got, "{[1,3),[5,8)}");
    // Single range.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT int4multirange(int4range(1, 3))::text"
        )),
        "{[1,3)}"
    );
}

#[test]
fn empty_ranges_drop_and_zero_args_empty() {
    let mut e = Engine::new();
    // Empty member ranges are dropped.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT int4multirange(int4range(1, 3), int4range(5, 5))::text"
        )),
        "{[1,3)}"
    );
    // Zero args → the empty multirange.
    assert_eq!(text(&first(&mut e, "SELECT int4multirange()::text")), "{}");
}

#[test]
fn kind_mismatch_errors() {
    let mut e = Engine::new();
    let err = e
        .execute("SELECT int4multirange(int8range(1, 3))")
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("doesn't match"), "unexpected error: {msg}");
}

#[test]
fn other_kinds_construct() {
    let mut e = Engine::new();
    let got = text(&first(
        &mut e,
        "SELECT datemultirange(daterange(DATE '2003-01-01', DATE '2003-02-01'))::text",
    ));
    assert!(
        got.starts_with('{') && got.contains("2003-01-01"),
        "unexpected render: {got}"
    );
}

#[test]
fn multirange_constructor_normalizes() {
    // PG normalizes multirange members: overlapping/adjacent spans merge,
    // empties drop, and the result is sorted. All live-PG18.4-verified.
    let mut e = Engine::new();
    let mr = |e: &mut Engine, expr: &str| text(&first(e, &format!("SELECT ({expr})::text")));
    // Overlap and (int4-discrete) adjacency both merge.
    assert_eq!(mr(&mut e, "int4multirange(int4range(1,5),int4range(4,8))"), "{[1,8)}");
    assert_eq!(mr(&mut e, "int4multirange(int4range(1,5),int4range(5,8))"), "{[1,8)}");
    // Disjoint spans stay separate; unsorted input is sorted.
    assert_eq!(mr(&mut e, "int4multirange(int4range(1,3),int4range(5,8))"), "{[1,3),[5,8)}");
    assert_eq!(mr(&mut e, "int4multirange(int4range(10,20),int4range(1,5))"), "{[1,5),[10,20)}");
    // Empty member dropped; continuous (numeric) overlap merges.
    assert_eq!(mr(&mut e, "int4multirange(int4range(1,5),int4range(3,3))"), "{[1,5)}");
    assert_eq!(mr(&mut e, "nummultirange(numrange(1,5),numrange(4,8))"), "{[1,8)}");
}
