//! to_char(number, format) — the numeric form (9/0 digit slots,
//! decimal point, thousands comma, FM fill mode, sign).

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    match &rows[0].values[0] {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("{sql}: expected Text, got {other:?}"),
    }
}

#[test]
fn fill_mode_forms() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT to_char(1234.56, 'FM9999.00')"), "1234.56");
    // Zero slots force leading zeros.
    assert_eq!(text(&mut e, "SELECT to_char(42, 'FM000')"), "042");
    assert_eq!(text(&mut e, "SELECT to_char(7, 'FM9')"), "7");
    // Rounds to the requested scale.
    assert_eq!(text(&mut e, "SELECT to_char(3.14159, 'FM9.999')"), "3.142");
    // The 0 slot after the point keeps its zeros.
    assert_eq!(text(&mut e, "SELECT to_char(0, 'FM990.00')"), "0.00");
}

#[test]
fn grouping_and_sign() {
    let mut e = Engine::new();
    // Fixed-width form reserves a leading blank for the sign; PG
    // right-aligns the grouped digits under the full field width
    // (three leading spaces here, verified against PG 18).
    assert_eq!(
        text(&mut e, "SELECT to_char(1234.5, '999,999.99')"),
        "   1,234.50"
    );
    assert_eq!(
        text(&mut e, "SELECT to_char(1234567, 'FM9,999,999')"),
        "1,234,567"
    );
    // Negative gets a leading minus even under FM.
    assert_eq!(text(&mut e, "SELECT to_char(-5, 'FM999')"), "-5");
}

#[test]
fn date_form_unaffected() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT to_char(DATE '2024-03-05', 'YYYY-MM-DD')"),
        "2024-03-05"
    );
}
