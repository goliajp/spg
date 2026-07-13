//! v7.39 (read01 utils/adt, formatting.c) — to_char / to_date /
//! to_timestamp / to_number gaps found by differential against PG18:
//! era-year rendering (ADJUST_YEAR), leading SG, locale currency L,
//! Julian / Roman-month / ISO-week-date / SSSS input fields, the 1 BC
//! default year, and format-driven to_number sign channels (PR / MI /
//! RN). All values byte-locked against PG18.

use spg_engine::{Engine, QueryResult};

fn row_of(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows[0]
            .values
            .iter()
            .map(spg_engine::eval::value_to_text)
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn to_char_bc_years_render_era_year() {
    let mut e = Engine::new();
    // 44 BC is astronomical -43; year tokens print the ERA year.
    assert_eq!(
        row_of(
            &mut e,
            "SELECT to_char(DATE '0044-03-15 BC', 'YYYY'), \
             to_char(DATE '0044-03-15 BC', 'YYYY AD'), \
             to_char(DATE '0044-03-15 BC', 'YY Y')"
        ),
        vec!["0044", "0044 BC", "44 4"]
    );
}

#[test]
fn to_char_numeric_sg_and_currency() {
    let mut e = Engine::new();
    // Leading SG always writes the sign itself (no blank column); the
    // locale currency L is a space in the C locale, a literal $ stays.
    assert_eq!(
        row_of(
            &mut e,
            "SELECT to_char(12, 'SG99'), to_char(-12, 'SG99'), \
             to_char(12345.678, 'L99G999D99')"
        ),
        vec!["+12", "-12", "  12,345.68"]
    );
}

#[test]
fn to_date_julian_roman_month_iso_week() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT to_date('2460379', 'J'), to_date('IV 2024', 'RM YYYY'), \
             to_date('2024-10-1', 'IYYY-IW-ID'), to_date('2024-10', 'IYYY-IW')"
        ),
        vec!["2024-03-09", "2024-04-01", "2024-03-04", "2024-03-04"]
    );
    // Bare IYYY stays at Jan 1 (PG leaves ww unset).
    assert_eq!(
        row_of(&mut e, "SELECT to_date('2016', 'IYYY')"),
        vec!["2016-01-01"]
    );
}

#[test]
fn to_timestamp_ssss_and_default_year_is_1_bc() {
    let mut e = Engine::new();
    // SSSS = seconds past midnight; a format with no year defaults to
    // year 0 (1 BC), like PG's ZERO_tm.
    assert_eq!(
        row_of(&mut e, "SELECT to_timestamp('50000', 'SSSS')::text"),
        vec!["0001-01-01 13:53:20+00 BC"]
    );
}

#[test]
fn to_number_sign_channels() {
    let mut e = Engine::new();
    // <n> is negative only under PR; a trailing sign needs MI/S/SG/PL.
    assert_eq!(
        row_of(
            &mut e,
            "SELECT to_number('<123>', '999PR'), to_number('1234-', '9999MI'), \
             to_number('5.01-', 'FM9.999999MI'), to_number('-12.5', 'S99D9')"
        ),
        vec!["-123", "-1234", "-5.01", "-12.5"]
    );
    // Without MI, a trailing minus is ignored (PG reads only the digits).
    assert_eq!(
        row_of(&mut e, "SELECT to_number('1234-', '9999')"),
        vec!["1234"]
    );
}

#[test]
fn to_number_roman() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT to_number('CDLXXXV', 'RN'), to_number('mmxxiv', 'RN'), \
             to_number('  XIV', 'RN')"
        ),
        vec!["485", "2024", "14"]
    );
    // Digits are not a Roman numeral; malformed forms are rejected.
    for bad in ["485", "IIII", "VV", "MCCM", "IL"] {
        let err = e
            .execute(&format!("SELECT to_number('{bad}', 'RN')"))
            .unwrap_err();
        assert!(
            format!("{err}").contains("invalid Roman numeral"),
            "{bad}: {err}"
        );
    }
}
