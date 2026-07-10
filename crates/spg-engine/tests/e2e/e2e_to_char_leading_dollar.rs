//! v7.38 (read01 sweep) — to_char numeric format: a leading literal `$`
//! (`FM$9,999.00`) anchors the dollar sign at the front, matching PG. Only a
//! trailing `$` was handled before. Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            v => format!("{v:?}"),
        },
        _ => panic!("expected rows"),
    }
}

#[test]
fn to_char_leading_dollar_currency() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT to_char(1234.5, 'FM$9,999.00')"),
        "$1,234.50"
    );
    assert_eq!(text(&mut e, "SELECT to_char(5, 'FM$99')"), "$5");
    // Without FM the field keeps its sign-column space (PG: '$ 1,234.50').
    assert_eq!(
        text(&mut e, "SELECT to_char(1234.5, '$9,999.00')"),
        "$ 1,234.50"
    );
    // A trailing `$` still renders at the end.
    assert_eq!(
        text(&mut e, "SELECT to_char(1234.5, 'FM9,999.00$')"),
        "1,234.50$"
    );
}
