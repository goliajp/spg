//! SQL-standard TRIM([BOTH|LEADING|TRAILING] [chars] FROM str) +
//! POSITION(sub IN str) syntactic forms + NUMERIC-operand interval
//! scaling.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn text(e: &mut Engine, sql: &str) -> String {
    match one(e, sql) {
        spg_storage::Value::Text(s) => s.into_owned(),
        other => panic!("{sql}: expected Text, got {other:?}"),
    }
}

#[test]
fn trim_from_forms() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT trim(BOTH 'x' FROM 'xxaxx')"), "a");
    assert_eq!(text(&mut e, "SELECT trim(LEADING 'x' FROM 'xxaxx')"), "axx");
    assert_eq!(text(&mut e, "SELECT trim(TRAILING 'x' FROM 'xxaxx')"), "xxa");
    // Keyword-less chars form + bare FROM (whitespace default).
    assert_eq!(text(&mut e, "SELECT trim('x' FROM 'xxaxx')"), "a");
    assert_eq!(text(&mut e, "SELECT trim(BOTH FROM '  a  ')"), "a");
    assert_eq!(text(&mut e, "SELECT trim(FROM '  a  ')"), "a");
    // Plain comma forms keep working.
    assert_eq!(text(&mut e, "SELECT trim('  a  ')"), "a");
    assert_eq!(text(&mut e, "SELECT trim('xxaxx', 'x')"), "a");
}

#[test]
fn position_in() {
    let mut e = Engine::new();
    let as_i64 = |v: spg_storage::Value<'_>| match v {
        spg_storage::Value::Int(n) => i64::from(n),
        spg_storage::Value::BigInt(n) => n,
        other => panic!("expected integer, got {other:?}"),
    };
    assert_eq!(as_i64(one(&mut e, "SELECT position('b' IN 'abc')")), 2);
    assert_eq!(as_i64(one(&mut e, "SELECT position('' IN 'abc')")), 1);
    assert_eq!(as_i64(one(&mut e, "SELECT position('z' IN 'abc')")), 0);
    // Regular IN-list membership is unaffected by the needle parse.
    let r = e.execute("SELECT 1 WHERE 2 IN (1, 2)").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    assert_eq!(rows.len(), 1);
}

#[test]
fn numeric_operand_interval_scaling() {
    let mut e = Engine::new();
    let iv = |v: spg_storage::Value<'static>| match v {
        spg_storage::Value::Interval {
            months,
            days,
            micros,
        } => (months, days, micros),
        other => panic!("expected Interval, got {other:?}"),
    };
    // 1.5::numeric is 1.5 (unconstrained numeric keeps its scale),
    // so 2 hours * 1.5 = 3 hours.
    assert_eq!(
        iv(one(&mut e, "SELECT INTERVAL '2 hours' * 1.5::numeric")),
        (0, 0, 3 * 3_600_000_000)
    );
    assert_eq!(
        iv(one(&mut e, "SELECT 1.5::numeric * INTERVAL '2 hours'")),
        (0, 0, 3 * 3_600_000_000)
    );
    assert_eq!(
        iv(one(&mut e, "SELECT INTERVAL '1 hour' / 2.0::numeric")),
        (0, 0, 1_800_000_000)
    );
}
