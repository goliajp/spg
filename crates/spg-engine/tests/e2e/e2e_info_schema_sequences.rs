//! v7.37.17 (17.6 siblings) — information_schema.sequences view.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.into_iter()
        .map(|row| row.values.into_iter().collect())
        .collect()
}

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn sequences_view_lists_declared_bounds() {
    let mut e = Engine::new();
    e.execute(
        "CREATE SEQUENCE seq_a START WITH 5 INCREMENT BY 2 MAXVALUE 1000 CYCLE",
    )
    .unwrap();
    e.execute("CREATE SEQUENCE seq_b").unwrap();
    let got = rows(
        &mut e,
        "SELECT sequence_name, start_value, increment, maximum_value, cycle_option \
         FROM information_schema.sequences ORDER BY sequence_name",
    );
    assert_eq!(got.len(), 2);
    assert_eq!(text(&got[0][0]), "seq_a");
    assert!(matches!(got[0][1], spg_storage::Value::BigInt(5)));
    assert!(matches!(got[0][2], spg_storage::Value::BigInt(2)));
    assert!(matches!(got[0][3], spg_storage::Value::BigInt(1000)));
    assert_eq!(text(&got[0][4]), "YES");
    assert_eq!(text(&got[1][0]), "seq_b");
    assert!(matches!(got[1][1], spg_storage::Value::BigInt(1)));
    assert_eq!(text(&got[1][4]), "NO");
}

#[test]
fn sequences_view_empty_without_sequences() {
    let mut e = Engine::new();
    let got = rows(&mut e, "SELECT * FROM information_schema.sequences");
    assert!(got.is_empty());
}
