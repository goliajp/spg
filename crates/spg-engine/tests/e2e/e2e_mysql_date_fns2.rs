//! v7.37.17 (17.6 siblings) — MySQL date functions batch 2:
//! quarter / to_days / from_days / makedate / yearweek.

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

fn int(v: &spg_storage::Value<'_>) -> i32 {
    match v {
        spg_storage::Value::Int(n) => *n,
        other => panic!("expected Int, got {other:?}"),
    }
}

#[test]
fn quarter_of_month() {
    let mut e = Engine::new();
    // MySQL doc vector: QUARTER('2008-04-01') = 2.
    assert_eq!(int(&first(&mut e, "SELECT quarter('2008-04-01')")), 2);
    assert_eq!(int(&first(&mut e, "SELECT quarter('2008-01-31')")), 1);
    assert_eq!(int(&first(&mut e, "SELECT quarter('2008-12-31')")), 4);
}

#[test]
fn to_days_from_days_roundtrip() {
    let mut e = Engine::new();
    // MySQL doc vector: TO_DAYS('2007-10-07') = 733321.
    assert!(matches!(
        first(&mut e, "SELECT to_days('2007-10-07')"),
        spg_storage::Value::BigInt(733_321)
    ));
    // Epoch: TO_DAYS('1970-01-01') = 719528.
    assert!(matches!(
        first(&mut e, "SELECT to_days('1970-01-01')"),
        spg_storage::Value::BigInt(719_528)
    ));
    // Roundtrip.
    assert_eq!(
        first(&mut e, "SELECT from_days(733321)"),
        first(&mut e, "SELECT make_date(2007, 10, 7)"),
    );
}

#[test]
fn makedate_from_dayofyear() {
    let mut e = Engine::new();
    // MySQL doc vectors: MAKEDATE(2011, 31) = '2011-01-31',
    // MAKEDATE(2011, 32) = '2011-02-01'.
    assert_eq!(
        first(&mut e, "SELECT makedate(2011, 31)"),
        first(&mut e, "SELECT make_date(2011, 1, 31)"),
    );
    assert_eq!(
        first(&mut e, "SELECT makedate(2011, 32)"),
        first(&mut e, "SELECT make_date(2011, 2, 1)"),
    );
    // dayofyear 0 → NULL.
    assert!(matches!(
        first(&mut e, "SELECT makedate(2011, 0)"),
        spg_storage::Value::Null
    ));
}

#[test]
fn yearweek_mode0() {
    let mut e = Engine::new();
    // MySQL doc vector: YEARWEEK('1987-01-01') = 198652 —
    // days before the year's first Sunday belong to the previous
    // year's week count.
    assert_eq!(
        int(&first(&mut e, "SELECT yearweek('1987-01-01')")),
        198_652
    );
    // A mid-year date: 2000-01-02 was a Sunday → week 1 of 2000.
    assert_eq!(
        int(&first(&mut e, "SELECT yearweek('2000-01-02')")),
        200_001
    );
}

#[test]
fn mysql_date2_null_passthrough() {
    let mut e = Engine::new();
    for f in &[
        "quarter(NULL::text)",
        "to_days(NULL::text)",
        "from_days(NULL::int)",
        "makedate(NULL::int, 1)",
        "yearweek(NULL::text)",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
