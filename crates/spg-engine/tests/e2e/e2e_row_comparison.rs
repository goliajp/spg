//! Row constructor comparisons + row IN — parse-time
//! lexicographic / row-equality expansion.

use spg_engine::{Engine, QueryResult};

fn b(e: &mut Engine, sql: &str) -> bool {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    match &rows[0].values[0] {
        spg_storage::Value::Bool(x) => *x,
        other => panic!("expected bool, got {other:?}"),
    }
}

#[test]
fn lexicographic_ordering() {
    let mut e = Engine::new();
    assert!(b(&mut e, "SELECT (1, 2) < (1, 3)"));
    assert!(!b(&mut e, "SELECT (1, 3) < (1, 3)"));
    assert!(b(&mut e, "SELECT (1, 3) <= (1, 3)"));
    assert!(b(&mut e, "SELECT (2, 0) > (1, 9)"));
    assert!(b(&mut e, "SELECT (1, 2, 3) < (1, 2, 4)"));
    assert!(b(&mut e, "SELECT (1, 'a') = (1, 'a')"));
    assert!(b(&mut e, "SELECT (1, 'a') <> (1, 'b')"));
}

#[test]
fn row_in_filters_rows() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE rc (v INT, t TEXT)").unwrap();
    e.execute("INSERT INTO rc VALUES (1, 'a'), (2, 'b'), (3, 'c')")
        .unwrap();
    let QueryResult::Rows { rows, .. } = e
        .execute("SELECT v FROM rc WHERE (v, t) IN ((1, 'a'), (3, 'c')) ORDER BY v")
        .unwrap()
    else {
        panic!("expected Rows");
    };
    assert_eq!(rows.len(), 2);
    assert!(matches!(rows[1].values[0], spg_storage::Value::Int(3)));
    // NOT IN complements.
    let QueryResult::Rows { rows, .. } = e
        .execute("SELECT v FROM rc WHERE (v, t) NOT IN ((1, 'a'), (3, 'c'))")
        .unwrap()
    else {
        panic!("expected Rows");
    };
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0].values[0], spg_storage::Value::Int(2)));
}

#[test]
fn arity_mismatch_errors() {
    let mut e = Engine::new();
    let err = e.execute("SELECT (1, 2) < (1, 2, 3)").unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("arity"), "unexpected error: {msg}");
}
