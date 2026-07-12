//! v7.37.17 (17.6 siblings) — cash_words(money) + to_ascii(text).

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

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn cash_words_pg_doc_vector() {
    let mut e = Engine::new();
    // PG regression-suite shape.
    assert_eq!(
        text(&first(&mut e, "SELECT cash_words('1.23')")),
        "One dollar and twenty three cents"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT cash_words('114.06')")),
        "One hundred fourteen dollars and six cents"
    );
}

#[test]
fn cash_words_integers_and_groups() {
    let mut e = Engine::new();
    assert_eq!(
        text(&first(&mut e, "SELECT cash_words(0)")),
        "Zero dollars and zero cents"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT cash_words(1)")),
        "One dollar and zero cents"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT cash_words(1234567)")),
        "One million two hundred thirty four thousand five hundred sixty seven dollars and zero cents"
    );
}

#[test]
fn cash_words_negative_and_dollar_sign() {
    let mut e = Engine::new();
    assert_eq!(
        text(&first(&mut e, "SELECT cash_words('-2.50')")),
        "Minus two dollars and fifty cents"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT cash_words('$1,234.56')")),
        "One thousand two hundred thirty four dollars and fifty six cents"
    );
}

#[test]
fn to_ascii_raises_pg_utf8_error() {
    // v7.39 (read01 utils/adt, ascii.c) — PG's to_ascii only converts
    // from LATIN1/2/9/WIN1250; in a UTF8 database it raises 0A000.
    // The old accent-stripping assertions locked an SPG invention.
    let mut e = Engine::new();
    let err = e.execute("SELECT to_ascii('Karél')").unwrap_err();
    assert!(
        format!("{err}").contains("encoding conversion from UTF8 to ASCII not supported"),
        "got {err}"
    );
}

#[test]
fn cash_words_to_ascii_null_passthrough() {
    let mut e = Engine::new();
    for f in &["cash_words(NULL::text)", "to_ascii(NULL::text)"] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
