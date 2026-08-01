//! v7.37.17 (17.6 siblings) — age(xid) overload + mxid_age for
//! autovacuum-wraparound monitoring queries.
//!
//! v7.39 (round 668) — this file used to assert `SELECT age(12345)` is 0,
//! under a comment naming the canonical shape as
//! `SELECT age(relfrozenxid) FROM pg_class`. Those are not the same call.
//! A bare integer is not an xid, PG refuses it outright, and the thing the
//! overload actually serves — a real `Value::Xid` — has its own arm that
//! computes a genuine distance from the current snapshot. So the test was
//! holding a divergence in place while describing the shape it was not
//! testing.
//!
//! Round 627 wrote the refusal, saw three tests go red, and read that as
//! evidence the overload needed integers. This file was two of them.

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

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).expect_err(sql))
}

/// The canonical shape, for real: a genuine xid, and a distance that moves
/// with the value. PG18-verified wording for the integer forms it refuses.
#[test]
fn age_xid_measures_a_real_distance() {
    let mut e = Engine::new();
    // An xid at or beyond the current counter is 0 away from it; an older
    // one is further. The exact counter is the engine's, so the assertion
    // is on the ORDERING, which is the property the monitoring query wants.
    let far = first(&mut e, "SELECT age('0'::xid)");
    let near = first(&mut e, "SELECT age('4294967295'::xid)");
    let (spg_storage::Value::Int(far), spg_storage::Value::Int(near)) = (far, near) else {
        panic!("age(xid) should be an integer, as on PG");
    };
    assert!(far >= near, "an older xid must not be nearer: {far} vs {near}");
    assert_eq!(near, 0, "an xid past the counter saturates at 0");
}

/// A bare integer is not an xid. PG18: `function age(integer) does not
/// exist`, and one message per width.
#[test]
fn age_refuses_a_bare_integer_the_way_pg_does() {
    let mut e = Engine::new();
    assert!(
        err(&mut e, "SELECT age(12345)").contains("function age(integer) does not exist"),
        "{}",
        err(&mut e, "SELECT age(12345)")
    );
    assert!(
        err(&mut e, "SELECT age(12345::bigint)").contains("function age(bigint) does not exist"),
        "{}",
        err(&mut e, "SELECT age(12345::bigint)")
    );
    // All three widths, because they do not travel the same way. A bare
    // literal reaches the refusal directly; `::smallint` is a NAMED cast;
    // `::int` and `::bigint` are their own CastTarget variants and were
    // missed by a name-only list, so `12345::bigint` was still answering
    // "age() needs DATE or TIMESTAMP" after `12345::smallint` was fixed.
    assert!(
        err(&mut e, "SELECT age(12345::smallint)")
            .contains("function age(smallint) does not exist"),
        "{}",
        err(&mut e, "SELECT age(12345::smallint)")
    );
    assert!(
        err(&mut e, "SELECT age(12345::int)").contains("function age(integer) does not exist"),
        "{}",
        err(&mut e, "SELECT age(12345::int)")
    );
    assert!(
        err(&mut e, "SELECT mxid_age(12345)")
            .contains("function mxid_age(integer) does not exist"),
        "{}",
        err(&mut e, "SELECT mxid_age(12345)")
    );
}

/// `mxid_age` keeps its 0, and that rationale is its own: SPG has no
/// multixact machinery, so there is no multixact age to report. It is NOT
/// the age(xid) rationale, which this file used to conflate them with.
#[test]
fn mxid_age_of_a_real_xid_is_zero() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT mxid_age('12345'::xid)"),
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

/// A NULL still passes through. Recorded divergence: PG resolves the
/// function before it looks at the value, so `age(NULL::int)` is a
/// "does not exist" error there and NULL here. SPG cannot tell
/// `NULL::int` from `NULL::xid` at this point — both arrive as
/// `Value::Null` — so closing it needs the static type, not this arm.
#[test]
fn age_xid_null_passthrough() {
    let mut e = Engine::new();
    for f in &["age(NULL::xid)", "mxid_age(NULL::xid)"] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
