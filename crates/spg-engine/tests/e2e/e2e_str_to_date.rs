//! v7.37.17 (17.6 siblings) — MySQL str_to_date + time_format.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
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
fn str_to_date_date_only() {
    let mut e = Engine::new();
    // MySQL doc vector: STR_TO_DATE('01,5,2013', '%d,%m,%Y')
    // → '2013-05-01' (a DATE — round-trip through date_format).
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT date_format(str_to_date('01,5,2013', '%d,%m,%Y'), '%Y-%m-%d')"
        )),
        "2013-05-01"
    );
    // Month name form.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT date_format(str_to_date('May 1, 2013', '%M %d, %Y'), '%Y-%m-%d')"
        )),
        "2013-05-01"
    );
}

#[test]
fn str_to_date_with_time_and_pm() {
    let mut e = Engine::new();
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT date_format(str_to_date('2013-05-01 09:30:17 PM', \
             '%Y-%m-%d %h:%i:%s %p'), '%Y-%m-%d %H:%i:%s')"
        )),
        "2013-05-01 21:30:17"
    );
}

#[test]
fn str_to_date_unparseable_is_null() {
    let mut e = Engine::new();
    // MySQL returns NULL (with a warning), not an error.
    assert!(matches!(
        first(&mut e, "SELECT str_to_date('not a date', '%Y-%m-%d')"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT str_to_date('2013-13-01', '%Y-%m-%d')"),
        spg_storage::Value::Null
    ));
}

#[test]
fn time_format_vectors() {
    let mut e = Engine::new();
    // MySQL doc vector: TIME_FORMAT('100:00:00', '%H %i %s') has
    // >24h semantics we don't model; the in-day form:
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT time_format('19:30:10', '%h %i %s %p')"
        )),
        "07 30 10 PM"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT time_format('09:05:00', '%H:%i')")),
        "09:05"
    );
}
