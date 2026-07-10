//! v7.37.17 (17.6 siblings) — MySQL clock spellings (curdate /
//! curtime / sysdate / utc_*) + adddate / subdate / date_sub.

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
fn clock_spellings_resolve() {
    // 2023-11-14 22:13:20 UTC — same pinned instant as
    // e2e_clock_family (the clock rewrite only fires when the host
    // installs a clock; Engine::new() alone stays deterministic).
    let mut e = Engine::new().with_clock(|| 1_700_000_000_000_000);
    // SPG's unified clock: curdate/utc_date read the same instant.
    let cur = first(&mut e, "SELECT curdate()");
    assert!(matches!(cur, spg_storage::Value::Date(_)));
    assert_eq!(first(&mut e, "SELECT utc_date()"), cur);
    assert!(matches!(
        first(&mut e, "SELECT sysdate()"),
        spg_storage::Value::Timestamp(1_700_000_000_000_000)
    ));
    assert!(matches!(
        first(&mut e, "SELECT utc_timestamp()"),
        spg_storage::Value::Timestamp(1_700_000_000_000_000)
    ));
    // Time-of-day text: 1_700_000_000 % 86400 = 80000s = 22:13:20.
    assert_eq!(text(&first(&mut e, "SELECT curtime()")), "22:13:20");
    assert_eq!(text(&first(&mut e, "SELECT utc_time()")), "22:13:20");
}

#[test]
fn adddate_bare_days_and_interval() {
    let mut e = Engine::new();
    // MySQL doc vector: ADDDATE('2008-01-02', 31) → '2008-02-02'.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT date_format(adddate('2008-01-02', 31), '%Y-%m-%d')"
        )),
        "2008-02-02"
    );
    // Interval form.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT date_format(adddate('2008-01-02', INTERVAL '1 day'), '%Y-%m-%d')"
        )),
        "2008-01-03"
    );
}

#[test]
fn subdate_and_date_sub() {
    let mut e = Engine::new();
    // MySQL doc vector: SUBDATE('2008-01-02', 31) → '2007-12-02'.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT date_format(subdate('2008-01-02', 31), '%Y-%m-%d')"
        )),
        "2007-12-02"
    );
    // MySQL doc vector shape: DATE_SUB('2005-01-01 00:00:00',
    // INTERVAL '1 second') → '2004-12-31 23:59:59'.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT date_format(date_sub('2005-01-01 00:00:00', \
             INTERVAL '1 second'), '%Y-%m-%d %H:%i:%s')"
        )),
        "2004-12-31 23:59:59"
    );
}
