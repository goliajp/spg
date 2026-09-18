//! 9.0.0 — an OID cast to `regclass` is a regclass, not its name in text.
//!
//! The cast rendered right and was wrong for everything else. Measured
//! against PostgreSQL 18.6:
//!
//! ```text
//!                                    PG 18.6     SPG 8.0.4
//!   pg_typeof(<oid>::regclass)       regclass    text
//!   (<oid>::regclass)::bigint        <oid>       invalid input syntax
//!                                                for type bigint: "zz_last"
//!   ORDER BY oid::regclass           by OID      by NAME
//!   999999::regclass                 999999      999999 (typed text)
//! ```
//!
//! The sort is the one that gives a wrong ANSWER: with oids 21680 and
//! 21683, PostgreSQL orders `zz_last, aa_first` and SPG ordered
//! `aa_first, zz_last`. The NAME cast already answered a regclass; this
//! is the same object arriving from the other side.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn run(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    else {
        panic!("expected rows for {sql}");
    };
    rows.into_iter()
        .map(|r| match r.values.into_iter().next().expect("one column") {
            // 9.0.0 — `pg_typeof` answers a `regtype`, which carries the oid beside the name (PostgreSQL 18.6 describes it as `regtype` and sends the oid in binary). What this pin means is the NAME.
            Value::Text(s) => s.to_string(),
            Value::RegClass(_, s) | Value::RegType(_, s) | Value::RegProc(_, s) => s.to_string(),
            Value::BigInt(n) => n.to_string(),
            Value::Int(n) => n.to_string(),
            other => panic!("unexpected {other:?}"),
        })
        .collect()
}

fn one(e: &mut Engine, sql: &str) -> String {
    let mut c = col(e, sql);
    assert_eq!(c.len(), 1, "{sql}");
    c.pop().expect("one row")
}

#[test]
fn an_oid_cast_to_regclass_keeps_its_oid() {
    let mut e = Engine::new();
    run(&mut e, "CREATE TABLE zz_last(i int)");
    run(&mut e, "CREATE TABLE aa_first(i int)");

    let zz: i64 = one(&mut e, "SELECT oid FROM pg_class WHERE relname = 'zz_last'")
        .parse()
        .expect("an oid");
    let aa: i64 = one(
        &mut e,
        "SELECT oid FROM pg_class WHERE relname = 'aa_first'",
    )
    .parse()
    .expect("an oid");
    // The premise the sort below rests on: the alphabetical order and
    // the oid order disagree.
    assert!(
        zz < aa,
        "zz_last {zz} should have been created first, aa_first {aa}"
    );

    assert_eq!(
        one(&mut e, &format!("SELECT pg_typeof({zz}::regclass)")),
        "regclass"
    );
    assert_eq!(
        one(&mut e, &format!("SELECT {zz}::regclass::bigint")),
        zz.to_string()
    );
    assert_eq!(one(&mut e, &format!("SELECT {zz}::regclass")), "zz_last");

    // The wrong ANSWER: ORDER BY a regclass is by oid, so the table made
    // first comes first however it is spelled.
    assert_eq!(
        col(
            &mut e,
            "SELECT oid::regclass FROM pg_class \
             WHERE relname IN ('zz_last','aa_first') ORDER BY oid::regclass"
        ),
        vec!["zz_last".to_string(), "aa_first".to_string()]
    );

    // An oid that names nothing is still a regclass, and renders as the
    // number — as PostgreSQL does for a dropped relation's oid.
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(999999::regclass)"),
        "regclass"
    );
    assert_eq!(one(&mut e, "SELECT 999999::regclass"), "999999");
    assert_eq!(one(&mut e, "SELECT 999999::regclass::bigint"), "999999");

    // And the NAME cast, which already worked, still does.
    assert_eq!(
        one(&mut e, "SELECT pg_typeof('zz_last'::regclass)"),
        "regclass"
    );
    assert_eq!(
        one(&mut e, "SELECT 'zz_last'::regclass::bigint"),
        zz.to_string()
    );
}
