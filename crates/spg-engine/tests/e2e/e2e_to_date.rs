//! v7.37.17 (17.6 siblings) — to_date(text, fmt) +
//! to_timestamp(text, fmt) format-template parsing.

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

fn date_days(v: &spg_storage::Value<'_>) -> i32 {
    match v {
        spg_storage::Value::Date(d) => *d,
        other => panic!("expected Date, got {other:?}"),
    }
}

fn ts_micros(v: &spg_storage::Value<'_>) -> i64 {
    match v {
        spg_storage::Value::Timestamp(t) => *t,
        other => panic!("expected Timestamp, got {other:?}"),
    }
}

#[test]
fn to_date_iso_format() {
    let mut e = Engine::new();
    // 2000-01-01 is day 10957 since epoch (1970-01-01 = 0).
    assert_eq!(
        date_days(&first(&mut e, "SELECT to_date('2000-01-01', 'YYYY-MM-DD')")),
        10957
    );
    // Cross-check against make_date.
    assert_eq!(
        date_days(&first(&mut e, "SELECT to_date('2024-06-15', 'YYYY-MM-DD')")),
        date_days(&first(&mut e, "SELECT make_date(2024, 6, 15)")),
    );
}

#[test]
fn to_date_alternate_separators_and_order() {
    let mut e = Engine::new();
    assert_eq!(
        date_days(&first(&mut e, "SELECT to_date('15/06/2024', 'DD/MM/YYYY')")),
        date_days(&first(&mut e, "SELECT make_date(2024, 6, 15)")),
    );
    // Month name forms.
    assert_eq!(
        date_days(&first(
            &mut e,
            "SELECT to_date('15 Jun 2024', 'DD Mon YYYY')"
        )),
        date_days(&first(&mut e, "SELECT make_date(2024, 6, 15)")),
    );
    assert_eq!(
        date_days(&first(
            &mut e,
            "SELECT to_date('15 January 2024', 'DD Month YYYY')"
        )),
        date_days(&first(&mut e, "SELECT make_date(2024, 1, 15)")),
    );
}

#[test]
fn to_timestamp_text_format() {
    let mut e = Engine::new();
    // Epoch anchor: 2000-01-01 00:00:00 = 10957 days * 86400 s.
    let expect = 10_957i64 * 86_400 * 1_000_000 + (13 * 3600 + 30 * 60 + 45) * 1_000_000;
    assert_eq!(
        ts_micros(&first(
            &mut e,
            "SELECT to_timestamp('2000-01-01 13:30:45', 'YYYY-MM-DD HH24:MI:SS')"
        )),
        expect
    );
    // HH12 + PM meridiem.
    assert_eq!(
        ts_micros(&first(
            &mut e,
            "SELECT to_timestamp('2000-01-01 01:30:45 PM', 'YYYY-MM-DD HH12:MI:SS PM')"
        )),
        expect
    );
}

#[test]
fn to_date_out_of_range_errors() {
    let mut e = Engine::new();
    assert!(
        e.execute("SELECT to_date('2024-13-01', 'YYYY-MM-DD')")
            .is_err()
    );
    assert!(
        e.execute("SELECT to_date('2024-01-45', 'YYYY-MM-DD')")
            .is_err()
    );
    // Non-digits where digits expected.
    assert!(
        e.execute("SELECT to_date('abcd-01-01', 'YYYY-MM-DD')")
            .is_err()
    );
}

/// U17 (read01 A-group): a 2-digit `YY` year used to be rendered as
/// `2000 + v`, so `to_date('70', 'YY')` wrongly gave 2070. PG applies
/// `adjust_partial_year_to_2020` (pivot at 70), and supports the `YYY`
/// (3-digit) and `CC` (century) tokens too. Every expected value was
/// captured live from PG 18.4.
#[test]
fn to_date_partial_year_pivot_matches_pg18() {
    let mut e = Engine::new();
    let d = |e: &mut Engine, sql: &str| match first(e, &format!("SELECT ({sql})::text")) {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("{sql}: got {other:?}"),
    };
    // YY: < 70 → 2000s, >= 70 → 1900s (pivot at 70).
    assert_eq!(d(&mut e, "to_date('70-01-15','YY-MM-DD')"), "1970-01-15");
    assert_eq!(d(&mut e, "to_date('69-01-15','YY-MM-DD')"), "2069-01-15");
    assert_eq!(d(&mut e, "to_date('00-01-15','YY-MM-DD')"), "2000-01-15");
    assert_eq!(d(&mut e, "to_date('99-01-15','YY-MM-DD')"), "1999-01-15");
    assert_eq!(d(&mut e, "to_date('50-01-15','YY-MM-DD')"), "2050-01-15");
    // YYY: 3-digit fold ('970' → 1970).
    assert_eq!(d(&mut e, "to_date('970-01-15','YYY-MM-DD')"), "1970-01-15");
    // YYYY: literal, no pivot ('24' → year 24).
    assert_eq!(d(&mut e, "to_date('24-01-15','YYYY-MM-DD')"), "0024-01-15");
    assert_eq!(
        d(&mut e, "to_date('2024-01-15','YYYY-MM-DD')"),
        "2024-01-15"
    );
    // CC: century alone → first year of that century.
    assert_eq!(d(&mut e, "to_date('21-01-15','CC-MM-DD')"), "2001-01-15");
}

#[test]
fn to_date_null_passthrough() {
    let mut e = Engine::new();
    for f in &[
        "to_date(NULL::text, 'YYYY-MM-DD')",
        "to_date('2024-01-01', NULL::text)",
        "to_timestamp(NULL::text, 'YYYY-MM-DD')",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
