//! v7.38 (read01) — the DATE text parser accepts PG's year-first numeric forms
//! with non-zero-padded month/day and `-`, `/` or `.` separators
//! (`2020-1-5`, `2020/01/5`, `2020.1.05`), while still rejecting out-of-range
//! and malformed input. Every expected value / rejection is from live PG18.4.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            other => panic!("{sql}: expected Text, got {other:?}"),
        },
        other => panic!("{sql}: expected Rows, got {other:?}"),
    }
}

#[test]
fn accepts_non_zero_padded_and_alt_separators() {
    let mut e = Engine::new();
    for lit in [
        "2020-01-05",
        "2020-1-5",
        "2020-01-5",
        "2020-1-05",
        "2020/01/05",
        "2020/1/5",
        "2020.01.05",
        " 2020-1-5 ",
    ] {
        assert_eq!(
            one(&mut e, &format!("SELECT ('{lit}'::date)::text")),
            "2020-01-05",
            "{lit}"
        );
    }
    // Compact ISO and special values still work.
    assert_eq!(one(&mut e, "SELECT ('20200105'::date)::text"), "2020-01-05");
}

#[test]
fn accepts_month_name_forms_in_any_order() {
    let mut e = Engine::new();
    for lit in [
        "Jan 5, 2020",
        "January 5, 2020",
        "5 Jan 2020",
        "5 January 2020",
        "2020-Jan-05",
        "2020 Jan 5",
        "Jan 5 2020",
        "JAN 5, 2020",
        "jan 5, 2020",
        "5-Jan-2020",
        "2020-Jan-5",
        "Jan 5,2020",
    ] {
        assert_eq!(
            one(&mut e, &format!("SELECT ('{lit}'::date)::text")),
            "2020-01-05",
            "{lit}"
        );
    }
    assert_eq!(
        one(&mut e, "SELECT ('Feb 29, 2020'::date)::text"),
        "2020-02-29"
    );
    // Invalid month name, out-of-range day, non-leap Feb 29, incomplete → error.
    for lit in ["Foo 5, 2020", "Jan 32, 2020", "Feb 29, 2021", "Jan 2020"] {
        assert!(
            e.execute(&format!("SELECT '{lit}'::date")).is_err(),
            "{lit}"
        );
    }
}

#[test]
fn still_rejects_out_of_range_and_garbage() {
    let mut e = Engine::new();
    for lit in [
        "2020-13-01",     // month out of range
        "2020-01-32",     // day out of range
        "2021-02-29",     // not a leap year
        "2020-00-05",     // month 0
        "2020-1-5 extra", // trailing garbage
        "2020-1",         // incomplete
        "20-1-5",         // 2-digit year (needs DateStyle)
        "2020-1-5-6",     // too many fields
    ] {
        assert!(
            e.execute(&format!("SELECT '{lit}'::date")).is_err(),
            "{lit} should be rejected"
        );
    }
}
