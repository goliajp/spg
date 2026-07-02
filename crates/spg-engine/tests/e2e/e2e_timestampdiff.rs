//! v7.37.17 (17.6 siblings) — MySQL timestampdiff + timestampadd
//! (bare unit keywords lowered by the parser).

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

fn as_i64(v: &spg_storage::Value<'_>) -> i64 {
    match v {
        spg_storage::Value::Int(n) => i64::from(*n),
        spg_storage::Value::BigInt(n) => *n,
        other => panic!("expected integer, got {other:?}"),
    }
}

#[test]
fn timestampdiff_doc_vectors() {
    let mut e = Engine::new();
    // MySQL doc vector: TIMESTAMPDIFF(MONTH, '2003-02-01',
    // '2003-05-01') → 3.
    assert_eq!(
        as_i64(&first(
            &mut e,
            "SELECT timestampdiff(MONTH, '2003-02-01', '2003-05-01')"
        )),
        3
    );
    // MySQL doc vector: TIMESTAMPDIFF(YEAR, '2002-05-01',
    // '2001-01-01') → -1.
    assert_eq!(
        as_i64(&first(
            &mut e,
            "SELECT timestampdiff(YEAR, '2002-05-01', '2001-01-01')"
        )),
        -1
    );
    // MySQL doc vector: TIMESTAMPDIFF(MINUTE, '2003-02-01',
    // '2003-05-01 12:05:55') → 128885.
    assert_eq!(
        as_i64(&first(
            &mut e,
            "SELECT timestampdiff(MINUTE, '2003-02-01 00:00:00', \
             '2003-05-01 12:05:55')"
        )),
        128885
    );
    // Complete-units semantics: one day short of a month → 0.
    assert_eq!(
        as_i64(&first(
            &mut e,
            "SELECT timestampdiff(MONTH, '2003-02-01', '2003-02-28')"
        )),
        0
    );
}

#[test]
fn timestampadd_doc_vectors() {
    let mut e = Engine::new();
    // MySQL doc vector: TIMESTAMPADD(MINUTE, 1, '2003-01-02')
    // → '2003-01-02 00:01:00'.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT date_format(timestampadd(MINUTE, 1, '2003-01-02'), \
             '%Y-%m-%d %H:%i:%s')"
        )),
        "2003-01-02 00:01:00"
    );
    // MySQL doc vector: TIMESTAMPADD(WEEK, 1, '2003-01-02')
    // → '2003-01-09'.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT date_format(timestampadd(WEEK, 1, '2003-01-02'), '%Y-%m-%d')"
        )),
        "2003-01-09"
    );
    // Month-end clamp: Jan 31 + 1 month → Feb 28.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT date_format(timestampadd(MONTH, 1, '2003-01-31'), '%Y-%m-%d')"
        )),
        "2003-02-28"
    );
}
