//! v7.38 (read01 P6.01) — to_char(numeric, fmt) formats from the exact
//! (scaled, scale) rather than routing the value through f64, so
//! high-precision numerics keep every digit. Oracle values from live PG 18.4.

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
fn to_char_numeric_keeps_full_precision() {
    // Source the exact numeric via to_number: a bare numeric LITERAL is still
    // lexed through f64 today (that is a separate gap, read01 P1.05), so we
    // isolate to_char's own fidelity here.
    let mut e = Engine::new();
    // 18 significant digits — well past an f64's ~15-16.
    assert_eq!(
        text(
            &mut e,
            "SELECT to_char(to_number('123456789012345.678','FM999999999999999.999'), \
             'FM999999999999999.999')"
        ),
        "123456789012345.678"
    );
    // 20-digit value that used to overflow the format via f64 (#######).
    assert_eq!(
        text(
            &mut e,
            "SELECT to_char(to_number('999999999999999999.99','FM999999999999999999.99'), \
             'FM999999999999999999.99')"
        ),
        "999999999999999999.99"
    );
}

#[test]
fn to_char_numeric_rounds_half_away_from_zero() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT to_char(1.678::numeric, 'FM9.99')"),
        "1.68"
    );
    assert_eq!(text(&mut e, "SELECT to_char(2.5::numeric, 'FM9')"), "3");
    assert_eq!(
        text(&mut e, "SELECT to_char((-3.145)::numeric, 'FM9.99')"),
        "-3.15"
    );
}

#[test]
fn to_char_numeric_grouping_and_zero_pad_intact() {
    let mut e = Engine::new();
    assert_eq!(
        text(
            &mut e,
            "SELECT to_char(1234567.89::numeric, 'FM9,999,999.99')"
        ),
        "1,234,567.89"
    );
    assert_eq!(text(&mut e, "SELECT to_char(0.1::numeric, 'FM0.9')"), "0.1");
    // Float inputs still format through the f64 path unchanged.
    assert_eq!(
        text(&mut e, "SELECT to_char(3.14::float8, 'FM9.99')"),
        "3.14"
    );
}
