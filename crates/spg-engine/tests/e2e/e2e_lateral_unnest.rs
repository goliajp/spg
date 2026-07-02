//! Correlated unnest — unnest(t.arr_col) expands per outer row
//! through the wrapped lateral_subquery channel (#349 sibling).

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.iter().map(|row| row.values.to_vec()).collect()
}

fn as_i64(v: &spg_storage::Value<'_>) -> i64 {
    match v {
        spg_storage::Value::Int(n) => i64::from(*n),
        spg_storage::Value::BigInt(n) => *n,
        other => panic!("expected integer, got {other:?}"),
    }
}

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected text, got {other:?}"),
    }
}

#[test]
fn unnest_of_outer_array_column() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ua (id INT, tags TEXT[])").unwrap();
    e.execute(
        "INSERT INTO ua VALUES (1, ARRAY['a','b']), (2, ARRAY['c'])",
    )
    .unwrap();
    // Each outer row fans out into its own tags: 2 + 1 = 3.
    let got = rows(
        &mut e,
        "SELECT ua.id, tag FROM ua, unnest(ua.tags) AS t(tag) \
         ORDER BY ua.id, tag",
    );
    assert_eq!(got.len(), 3);
    assert_eq!((as_i64(&got[0][0]), text(&got[0][1])), (1, "a".into()));
    assert_eq!((as_i64(&got[1][0]), text(&got[1][1])), (1, "b".into()));
    assert_eq!((as_i64(&got[2][0]), text(&got[2][1])), (2, "c".into()));
}

#[test]
fn explicit_lateral_unnest_with_ordinality() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ub (id INT, xs TEXT[])").unwrap();
    e.execute("INSERT INTO ub VALUES (1, ARRAY['p','q'])").unwrap();
    let got = rows(
        &mut e,
        "SELECT v, i FROM ub, LATERAL unnest(ub.xs) \
         WITH ORDINALITY AS t(v, i) ORDER BY i",
    );
    assert_eq!(got.len(), 2);
    assert_eq!((text(&got[0][0]), as_i64(&got[0][1])), ("p".into(), 1));
    assert_eq!((text(&got[1][0]), as_i64(&got[1][1])), ("q".into(), 2));
}
