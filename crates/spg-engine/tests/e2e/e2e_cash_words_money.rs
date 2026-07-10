//! v7.38 (read01 P6.33) — cash_words() accepts the money type (i64 cents),
//! spelling an amount out in English words. Oracle values from live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            v => panic!("expected text, got {v:?}"),
        },
        _ => panic!("expected rows"),
    }
}

#[test]
fn cash_words_on_money() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT cash_words('0'::money)"),
        "Zero dollars and zero cents"
    );
    assert_eq!(
        text(&mut e, "SELECT cash_words('1.01'::money)"),
        "One dollar and one cent"
    );
    assert_eq!(
        text(&mut e, "SELECT cash_words('21.00'::money)"),
        "Twenty one dollars and zero cents"
    );
    assert_eq!(
        text(&mut e, "SELECT cash_words('1234.56'::money)"),
        "One thousand two hundred thirty four dollars and fifty six cents"
    );
    assert_eq!(
        text(&mut e, "SELECT cash_words('-5.00'::money)"),
        "Minus five dollars and zero cents"
    );
    assert_eq!(
        text(&mut e, "SELECT cash_words(12.34::money)"),
        "Twelve dollars and thirty four cents"
    );
}
