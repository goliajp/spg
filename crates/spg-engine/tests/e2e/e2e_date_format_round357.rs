//! read01 round 357 (MySQL differential, M17) — DATE_FORMAT's specifiers.
//!
//! Fifteen specifiers were echoed as bare letters, so a format string came
//! back with `W` where MariaDB writes `Monday`: %k %l %W %a %w %j %D %r
//! %T %U %u %V %v %X %x %Z. No error — just the letter, which is what
//! makes it worth pinning against measured output rather than "it runs".
//!
//! The week numbers were the part worth being careful about, and the
//! table below is the MariaDB 11 answer for nine dates chosen to hit
//! every boundary: a year starting on Monday (2024), one starting on
//! Sunday (2023), one starting on Friday (2021), a leap day, a year end
//! that belongs to the NEXT ISO year, and a year start that belongs to
//! the PREVIOUS one. All four week forms differ from each other on this
//! set:
//!
//!   * `%U` / `%u` count 00-based from the year's first Sunday / Monday;
//!   * `%V` / `%X` start week 1 at the year's FIRST SUNDAY, so
//!     2024-01-01 is week 53 of 2023;
//!   * `%v` / `%x` are ISO-8601, so 2021-01-01 is week 53 of 2020.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => match rows.first().and_then(|r| r.values.first()) {
            Some(Value::Text(t)) => t.to_string(),
            other => panic!("{sql}: {other:?}"),
        },
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

/// The whole specifier set against one instant, as MariaDB renders it.
#[test]
fn every_specifier_matches() {
    let mut e = mysql();
    let d = "'2024-01-15 14:05:09.123456'";
    assert_eq!(
        text(
            &mut e,
            &format!("SELECT DATE_FORMAT({d},'%Y|%y|%m|%c|%d|%e|%H|%k|%h|%I|%i|%s|%f')")
        ),
        "2024|24|01|1|15|15|14|14|02|02|05|09|123456",
    );
    assert_eq!(
        text(
            &mut e,
            &format!("SELECT DATE_FORMAT({d},'%W|%a|%M|%b|%j|%p|%r|%T|%D')")
        ),
        "Monday|Mon|January|Jan|015|PM|02:05:09 PM|14:05:09|15th",
    );
    assert_eq!(
        text(
            &mut e,
            &format!("SELECT DATE_FORMAT({d},'%U|%u|%V|%v|%X|%x|%w')")
        ),
        "02|03|02|03|2024|2024|1",
    );
    // `%%` is a literal percent; an unknown specifier keeps its letter.
    assert_eq!(
        text(&mut e, &format!("SELECT DATE_FORMAT({d},'%%|%Z|%q')")),
        "%|UTC|q",
    );
}

/// The measured table, row for row. This is what holds the four week
/// forms honest — they agree on most days and part company exactly here.
#[test]
fn the_week_numbers_match_across_the_boundaries() {
    let mut e = mysql();
    for (date, want) in [
        ("2024-01-01", "00|01|53|01|2023|2024|1|001|1st"),
        ("2024-01-07", "01|01|01|01|2024|2024|0|007|7th"),
        ("2024-01-15", "02|03|02|03|2024|2024|1|015|15th"),
        ("2023-01-01", "01|00|01|52|2023|2022|0|001|1st"),
        ("2024-12-31", "52|53|52|01|2024|2025|2|366|31st"),
        ("2024-02-29", "08|09|08|09|2024|2024|4|060|29th"),
        ("2024-03-02", "08|09|08|09|2024|2024|6|062|2nd"),
        ("2024-03-03", "09|09|09|09|2024|2024|0|063|3rd"),
        ("2021-01-01", "00|00|52|53|2020|2020|5|001|1st"),
    ] {
        assert_eq!(
            text(
                &mut e,
                &format!("SELECT DATE_FORMAT('{date}','%U|%u|%V|%v|%X|%x|%w|%j|%D')")
            ),
            want,
            "for {date}"
        );
    }
}

/// The ordinal suffix has its own edge cases.
#[test]
fn the_ordinal_suffix_is_english() {
    let mut e = mysql();
    for (day, want) in [
        ("2024-01-01", "1st"),
        ("2024-01-02", "2nd"),
        ("2024-01-03", "3rd"),
        ("2024-01-04", "4th"),
        ("2024-01-11", "11th"),
        ("2024-01-12", "12th"),
        ("2024-01-13", "13th"),
        ("2024-01-21", "21st"),
        ("2024-01-22", "22nd"),
        ("2024-01-23", "23rd"),
        ("2024-01-31", "31st"),
    ] {
        assert_eq!(
            text(&mut e, &format!("SELECT DATE_FORMAT('{day}','%D')")),
            want
        );
    }
}
