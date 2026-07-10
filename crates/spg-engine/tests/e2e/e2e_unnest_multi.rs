//! Multi-arg unnest(a, b, …) — parallel zip, NULL-padded to the
//! longest array (PG's ROWS FROM shorthand).

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
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
fn zip_pads_shorter_with_nulls() {
    let mut e = Engine::new();
    // 3-long text zips with 2-long int: third int cell is NULL.
    let got = rows(
        &mut e,
        "SELECT * FROM unnest(ARRAY['a','b','c'], ARRAY[10, 20]) AS t(s, n)",
    );
    assert_eq!(got.len(), 3);
    assert_eq!(text(&got[0][0]), "a");
    assert_eq!(as_i64(&got[0][1]), 10);
    assert_eq!(text(&got[2][0]), "c");
    assert!(matches!(&got[2][1], spg_storage::Value::Null));
}

#[test]
fn zip_with_ordinality_and_where() {
    let mut e = Engine::new();
    let got = rows(
        &mut e,
        "SELECT s, n, ordinality \
         FROM unnest(ARRAY['x','y'], ARRAY[1, 2]) WITH ORDINALITY AS t(s, n) \
         WHERE n > 1",
    );
    assert_eq!(got.len(), 1);
    assert_eq!(text(&got[0][0]), "y");
    assert_eq!(as_i64(&got[0][1]), 2);
    assert_eq!(as_i64(&got[0][2]), 2);
}

#[test]
fn zip_in_join_position() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE zj (k INT)").unwrap();
    e.execute("INSERT INTO zj VALUES (9)").unwrap();
    let got = rows(
        &mut e,
        "SELECT k, a, b FROM zj, unnest(ARRAY[1, 2], ARRAY['p','q']) AS t(a, b) \
         ORDER BY a",
    );
    assert_eq!(got.len(), 2);
    assert_eq!(as_i64(&got[1][1]), 2);
    assert_eq!(text(&got[1][2]), "q");
}
