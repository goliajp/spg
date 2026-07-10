//! v7.37.17 (17.6 siblings) — range bound predicates upgraded from
//! constant stubs to real text-form parsing + range_merge.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn boolean(v: &spg_storage::Value<'_>) -> bool {
    match v {
        spg_storage::Value::Bool(b) => *b,
        other => panic!("expected Bool, got {other:?}"),
    }
}

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn bound_inclusivity_reads_brackets() {
    let mut e = Engine::new();
    // Canonical int-range form '[a,b)'.
    assert!(boolean(&first(&mut e, "SELECT lower_inc('[1,10)')")));
    assert!(!boolean(&first(&mut e, "SELECT upper_inc('[1,10)')")));
    assert!(boolean(&first(&mut e, "SELECT upper_inc('(1,10]')")));
    assert!(!boolean(&first(&mut e, "SELECT lower_inc('(1,10]')")));
}

#[test]
fn infinite_bounds_detected() {
    let mut e = Engine::new();
    assert!(boolean(&first(&mut e, "SELECT lower_inf('(,10)')")));
    assert!(!boolean(&first(&mut e, "SELECT lower_inf('[1,10)')")));
    assert!(boolean(&first(&mut e, "SELECT upper_inf('[1,)')")));
    assert!(!boolean(&first(&mut e, "SELECT upper_inf('[1,10)')")));
    // PG: infinite bounds are never inclusive.
    assert!(!boolean(&first(&mut e, "SELECT lower_inc('[,10)')")));
}

#[test]
fn isempty_matches_empty_literal() {
    let mut e = Engine::new();
    assert!(boolean(&first(&mut e, "SELECT isempty('empty')")));
    assert!(!boolean(&first(&mut e, "SELECT isempty('[1,10)')")));
    // All bound predicates are false on the empty range.
    assert!(!boolean(&first(&mut e, "SELECT lower_inc('empty')")));
    assert!(!boolean(&first(&mut e, "SELECT upper_inf('empty')")));
}

#[test]
fn range_merge_smallest_containing() {
    let mut e = Engine::new();
    // PG doc vector: range_merge('[1,2)','[3,4)') = [1,4).
    assert_eq!(
        text(&first(&mut e, "SELECT range_merge('[1,2)', '[3,4)')")),
        "[1,4)"
    );
    // Overlapping.
    assert_eq!(
        text(&first(&mut e, "SELECT range_merge('[1,5)', '[3,9]')")),
        "[1,9]"
    );
    // Numeric compare, not lexicographic: 9 < 10.
    assert_eq!(
        text(&first(&mut e, "SELECT range_merge('[9,20)', '[10,30)')")),
        "[9,30)"
    );
    // Empty input returns the other range.
    assert_eq!(
        text(&first(&mut e, "SELECT range_merge('empty', '[3,4)')")),
        "[3,4)"
    );
    // Infinite bound propagates.
    assert_eq!(
        text(&first(&mut e, "SELECT range_merge('[1,)', '[3,4)')")),
        "[1,)"
    );
}

#[test]
fn range_predicates_null_passthrough() {
    let mut e = Engine::new();
    for f in &[
        "lower_inc(NULL::text)",
        "isempty(NULL::text)",
        "range_merge(NULL::text, '[1,2)')",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
