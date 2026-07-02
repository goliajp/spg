//! v7.37.17 (17.6 siblings) — MySQL FORMAT(X, D) numeric arm on the
//! format() dispatch + NAME_CONST.

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
fn mysql_format_doc_vectors() {
    let mut e = Engine::new();
    // MySQL doc vector: FORMAT(12332.123456, 4) → '12,332.1235'.
    assert_eq!(
        text(&first(&mut e, "SELECT format(12332.123456, 4)")),
        "12,332.1235"
    );
    // MySQL doc vector: FORMAT(12332.1, 4) → '12,332.1000'.
    assert_eq!(
        text(&first(&mut e, "SELECT format(12332.1, 4)")),
        "12,332.1000"
    );
    // MySQL doc vector: FORMAT(12332.2, 0) → '12,332'.
    assert_eq!(text(&first(&mut e, "SELECT format(12332.2, 0)")), "12,332");
    // Negative + million-scale grouping.
    assert_eq!(
        text(&first(&mut e, "SELECT format(-1234567, 0)")),
        "-1,234,567"
    );
}

#[test]
fn pg_printf_format_still_works() {
    let mut e = Engine::new();
    // The text-first-arg path stays PG printf-style.
    assert_eq!(
        text(&first(&mut e, "SELECT format('Hello %s', 'World')")),
        "Hello World"
    );
}

#[test]
fn name_const_returns_value() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT name_const('myname', 14)"),
        spg_storage::Value::Int(14) | spg_storage::Value::BigInt(14)
    ));
}
