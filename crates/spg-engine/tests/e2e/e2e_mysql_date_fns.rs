//! v7.37.17 (17.6 siblings) — MySQL date accessors: dayname /
//! monthname / dayofweek / dayofyear / weekofyear / last_day /
//! datediff / strcmp.

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

fn int(v: &spg_storage::Value<'_>) -> i32 {
    match v {
        spg_storage::Value::Int(n) => *n,
        other => panic!("expected Int, got {other:?}"),
    }
}

#[test]
fn dayname_monthname() {
    let mut e = Engine::new();
    // 2007-02-03 was a Saturday (MySQL doc vector).
    assert_eq!(
        text(&first(&mut e, "SELECT dayname('2007-02-03')")),
        "Saturday"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT monthname('2008-02-03')")),
        "February"
    );
    // Epoch anchor: 1970-01-01 was a Thursday.
    assert_eq!(
        text(&first(&mut e, "SELECT dayname('1970-01-01')")),
        "Thursday"
    );
}

#[test]
fn dayofweek_dayofyear_weekofyear() {
    let mut e = Engine::new();
    // MySQL doc vectors: DAYOFWEEK('2007-02-03') = 7 (Saturday).
    assert_eq!(int(&first(&mut e, "SELECT dayofweek('2007-02-03')")), 7);
    // DAYOFYEAR('2007-02-03') = 34.
    assert_eq!(int(&first(&mut e, "SELECT dayofyear('2007-02-03')")), 34);
    // WEEKOFYEAR('2008-02-20') = 8.
    assert_eq!(int(&first(&mut e, "SELECT weekofyear('2008-02-20')")), 8);
    // ISO week edge: 2021-01-01 (Friday) belongs to 2020-W53.
    assert_eq!(int(&first(&mut e, "SELECT weekofyear('2021-01-01')")), 53);
}

#[test]
fn last_day_clamps() {
    let mut e = Engine::new();
    // MySQL doc vectors, cross-checked via make_date.
    assert_eq!(
        first(&mut e, "SELECT last_day('2003-02-05')"),
        first(&mut e, "SELECT make_date(2003, 2, 28)"),
    );
    assert_eq!(
        first(&mut e, "SELECT last_day('2004-02-05')"),
        first(&mut e, "SELECT make_date(2004, 2, 29)"),
    );
    assert_eq!(
        first(&mut e, "SELECT last_day('2004-12-05')"),
        first(&mut e, "SELECT make_date(2004, 12, 31)"),
    );
}

#[test]
fn datediff_day_delta() {
    let mut e = Engine::new();
    // MySQL doc vector: DATEDIFF('2007-12-31','2007-12-30') = 1.
    assert_eq!(
        int(&first(
            &mut e,
            "SELECT datediff('2007-12-31', '2007-12-30')"
        )),
        1
    );
    assert_eq!(
        int(&first(
            &mut e,
            "SELECT datediff('2010-11-30', '2010-12-31')"
        )),
        -31
    );
}

#[test]
fn strcmp_three_way() {
    let mut e = Engine::new();
    assert_eq!(int(&first(&mut e, "SELECT strcmp('text', 'text2')")), -1);
    assert_eq!(int(&first(&mut e, "SELECT strcmp('text2', 'text')")), 1);
    assert_eq!(int(&first(&mut e, "SELECT strcmp('text', 'text')")), 0);
}

#[test]
fn mysql_date_null_passthrough() {
    let mut e = Engine::new();
    for f in &[
        "dayname(NULL::text)",
        "last_day(NULL::text)",
        "datediff(NULL::text, '2020-01-01')",
        "strcmp(NULL::text, 'x')",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
