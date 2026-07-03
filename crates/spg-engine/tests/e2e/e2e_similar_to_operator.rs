//! x [NOT] SIMILAR TO p [ESCAPE e] — inline operator over the
//! similar_to_escape + regexp_like lowering.

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
fn similar_to_doc_vectors() {
    let mut e = Engine::new();
    // PG doc vectors.
    assert!(!b(&mut e, "SELECT 'abc' SIMILAR TO 'abc_'"));
    assert!(b(&mut e, "SELECT 'abc' SIMILAR TO '%(b|d)%'"));
    assert!(!b(&mut e, "SELECT 'abc' SIMILAR TO '(b|c)%'"));
    // Full-string anchoring — LIKE-style partial match fails.
    assert!(!b(&mut e, "SELECT 'abcd' SIMILAR TO 'ab'"));
    assert!(b(&mut e, "SELECT 'abc' NOT SIMILAR TO 'xyz'"));
}

#[test]
fn similar_to_in_where_with_escape() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE st (t TEXT)").unwrap();
    e.execute("INSERT INTO st VALUES ('50% off'), ('50x off')")
        .unwrap();
    let QueryResult::Rows { rows, .. } = e
        .execute("SELECT t FROM st WHERE t SIMILAR TO '50!% off' ESCAPE '!'")
        .unwrap()
    else {
        panic!("expected Rows");
    };
    assert_eq!(rows.len(), 1);
    assert!(matches!(&rows[0].values[0], spg_storage::Value::Text(s) if s == "50% off"));
}
