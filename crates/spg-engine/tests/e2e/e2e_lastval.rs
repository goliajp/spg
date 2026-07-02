//! v7.37.17 (17.6 siblings) — lastval() upgraded from NULL stub to
//! real session-level last-sequence register.

use spg_engine::{Engine, QueryResult};

fn first_i64(e: &mut Engine, sql: &str) -> i64 {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    match &rows[0].values[0] {
        spg_storage::Value::SmallInt(n) => i64::from(*n),
        spg_storage::Value::Int(n) => i64::from(*n),
        spg_storage::Value::BigInt(n) => *n,
        other => panic!("{sql}: not an integer: {other:?}"),
    }
}

#[test]
fn lastval_follows_nextval() {
    let mut e = Engine::new();
    e.execute("CREATE SEQUENCE s1").unwrap();
    e.execute("CREATE SEQUENCE s2 START WITH 100").unwrap();
    assert_eq!(first_i64(&mut e, "SELECT nextval('s1')"), 1);
    assert_eq!(first_i64(&mut e, "SELECT lastval()"), 1);
    // Switches to the most recently used sequence.
    assert_eq!(first_i64(&mut e, "SELECT nextval('s2')"), 100);
    assert_eq!(first_i64(&mut e, "SELECT lastval()"), 100);
    // Advancing s1 again moves lastval back.
    assert_eq!(first_i64(&mut e, "SELECT nextval('s1')"), 2);
    assert_eq!(first_i64(&mut e, "SELECT lastval()"), 2);
}

#[test]
fn lastval_before_nextval_errors() {
    let mut e = Engine::new();
    // PG: ERROR: lastval is not yet defined in this session.
    assert!(e.execute("SELECT lastval()").is_err());
}
