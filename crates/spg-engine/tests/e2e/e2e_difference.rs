//! v7.37.17 (17.6 siblings) — fuzzystrmatch difference(a, b).

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

fn as_int(v: &spg_storage::Value<'_>) -> i32 {
    match v {
        spg_storage::Value::Int(n) => *n,
        other => panic!("expected Int, got {other:?}"),
    }
}

#[test]
fn difference_identical_soundex() {
    let mut e = Engine::new();
    // Both → R163
    assert_eq!(
        as_int(&first(&mut e, "SELECT difference('Robert', 'Rupert')")),
        4
    );
    // Same word → 4.
    assert_eq!(
        as_int(&first(&mut e, "SELECT difference('smith', 'smith')")),
        4
    );
}

#[test]
fn difference_partial_match() {
    let mut e = Engine::new();
    // smith (S530) vs snow (S500) — first 2 chars same (S5), 3rd
    // differs (3 vs 0), 4th differs (0 vs 0 = same).
    let n = as_int(&first(&mut e, "SELECT difference('smith', 'snow')"));
    assert!((2..=4).contains(&n), "got {n}");
}

#[test]
fn difference_different() {
    let mut e = Engine::new();
    // 'Robert' (R163) vs 'Xerxes' (X620) — different first letter,
    // different codes → 0 matching chars.
    assert_eq!(
        as_int(&first(&mut e, "SELECT difference('Robert', 'Xerxes')")),
        0
    );
}

#[test]
fn difference_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT difference(NULL::text, 'x')"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT difference('x', NULL::text)"),
        spg_storage::Value::Null
    ));
}
