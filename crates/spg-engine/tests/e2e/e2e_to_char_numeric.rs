//! to_char(number, format) — the numeric form (9/0 digit slots,
//! decimal point, thousands comma, FM fill mode, sign).

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
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
    assert_eq!(
        text(&mut e, "SELECT to_char(1234.56, 'FM9999.00')"),
        "1234.56"
    );
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

// v7.38 (read01, T23) — `EEEE` scientific-notation placement rules. Valid
// masks keep working; malformed ones error the way PG18.4 does instead of
// silently mis-rendering. Every expected string / error is from live PG18.4.
#[test]
fn eeee_scientific_valid_forms() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT to_char(12345.678, '9.999EEEE')"),
        " 1.235e+04"
    );
    assert_eq!(text(&mut e, "SELECT to_char(12345.678, '9EEEE')"), " 1e+04");
    assert_eq!(
        text(&mut e, "SELECT to_char(0.00012345, '9.99EEEE')"),
        " 1.23e-04"
    );
    assert_eq!(
        text(&mut e, "SELECT to_char(-12345.678, '9.999EEEE')"),
        "-1.235e+04"
    );
    assert_eq!(text(&mut e, "SELECT to_char(12345.678, 'EEEE')"), " 1e+04");
    // Group/decimal coexist with EEEE; a trailing quoted literal is accepted
    // but (like PG) contributes nothing to the scientific output.
    assert_eq!(
        text(&mut e, "SELECT to_char(12345.678, '9G999EEEE')"),
        " 1e+04"
    );
    assert_eq!(
        text(&mut e, "SELECT to_char(12345.678, '9.9EEEE\"z\"')"),
        " 1.2e+04"
    );
}

#[test]
fn eeee_incompatible_flags_error() {
    // A sign/fill/scale/roman flag anywhere before EEEE is rejected.
    let mut e = Engine::new();
    for m in [
        "FM9.999EEEE",
        "FMEEEE",
        "S9.9EEEE",
        "MI9.9EEEE",
        "PL9.9EEEE",
        "SG9.9EEEE",
        "B9.9EEEE",
        "RN EEEE",
        "V9EEEE",
        "S9.9EEEE9",
    ] {
        assert!(
            e.execute(&format!("SELECT to_char(12345.678, '{m}')"))
                .is_err(),
            "mask {m:?} must be rejected as EEEE-incompatible",
        );
    }
}

#[test]
fn eeee_must_be_last_error() {
    // Any format token after EEEE is rejected; literals are fine.
    let mut e = Engine::new();
    for m in [
        "9.9EEEE9",
        "EEEE9",
        "9.9EEEE.",
        "9.9EEEE,",
        "9.9EEEED",
        "9.9EEEEL",
        "9.9EEEEMI",
    ] {
        assert!(
            e.execute(&format!("SELECT to_char(12345.678, '{m}')"))
                .is_err(),
            "mask {m:?} must be rejected: EEEE not last",
        );
    }
    // A trailing literal (space / $ / quoted) is NOT an error; like PG it
    // contributes nothing to the scientific output.
    assert_eq!(
        text(&mut e, "SELECT to_char(12345.678, '9.9EEEE$')"),
        " 1.2e+04"
    );
}
