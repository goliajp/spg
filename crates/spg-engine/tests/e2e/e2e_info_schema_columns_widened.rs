//! v7.37.17 (17.6 siblings) — information_schema.columns widened to
//! the columns Alembic autogenerate / SQLAlchemy reflection read.

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
fn widened_columns_present() {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE wc (id BIGSERIAL, n INT DEFAULT 7, note TEXT)",
    )
    .unwrap();
    let got = rows(
        &mut e,
        "SELECT column_name, column_default, numeric_precision, udt_name, is_identity \
         FROM information_schema.columns WHERE table_name = 'wc' ORDER BY ordinal_position",
    );
    assert_eq!(got.len(), 3);
    // id: serial → nextval default + identity YES + int8 udt.
    assert_eq!(text(&got[0][0]), "id");
    assert!(
        text(&got[0][1]).contains("nextval"),
        "id default: {:?}",
        got[0][1]
    );
    assert_eq!(text(&got[0][3]), "int8");
    assert_eq!(text(&got[0][4]), "YES");
    // n: literal default 7, precision 32, int4.
    assert_eq!(text(&got[1][0]), "n");
    assert_eq!(text(&got[1][1]), "7");
    assert!(matches!(got[1][2], spg_storage::Value::Int(32)));
    assert_eq!(text(&got[1][3]), "int4");
    assert_eq!(text(&got[1][4]), "NO");
    // note: no default → NULL, text udt.
    assert_eq!(text(&got[2][0]), "note");
    assert!(matches!(got[2][1], spg_storage::Value::Null));
    assert_eq!(text(&got[2][3]), "text");
}
