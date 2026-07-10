//! v7.37.17 (17.6 siblings) — age(xid) overload + mxid_age for
//! autovacuum-wraparound monitoring queries.

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

#[test]
fn age_xid_returns_zero() {
    let mut e = Engine::new();
    // The canonical wraparound-monitoring shape:
    //   SELECT age(relfrozenxid) FROM pg_class ...
    // SPG's u64 tx ids never wrap → distance honestly 0.
    assert!(matches!(
        first(&mut e, "SELECT age(12345)"),
        spg_storage::Value::Int(0)
    ));
    assert!(matches!(
        first(&mut e, "SELECT mxid_age(12345)"),
        spg_storage::Value::Int(0)
    ));
}

#[test]
fn age_timestamp_form_unchanged() {
    let mut e = Engine::new();
    // The 2-arg timestamp form still computes a real interval.
    assert!(matches!(
        first(
            &mut e,
            "SELECT age('2020-01-02'::timestamp, '2020-01-01'::timestamp)"
        ),
        spg_storage::Value::Interval { .. }
    ));
}

#[test]
fn age_xid_null_passthrough() {
    let mut e = Engine::new();
    for f in &["age(NULL::int)", "mxid_age(NULL::int)"] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
