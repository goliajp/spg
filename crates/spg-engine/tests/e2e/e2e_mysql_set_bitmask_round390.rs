//! read01 round 390 (MySQL type-fidelity epic — P5) — a SET column reads
//! as its bitmask in a numeric / bitwise context.
//!
//! MariaDB stores a SET underneath as an integer bitmask (member N is bit
//! N of the declared variant list) and shows the text on read. So `s + 0`
//! is the mask (`'a,c'` over `('a','b','c','d')` is 1 | 4 = 5) and
//! `WHERE s & flag` filters by membership. SPG stores SET as text, so the
//! numeric path coerced `'a,c'` to 0 — `s + 0` was 0 and `WHERE s & 2`
//! matched NOTHING (a silent-wrong: wrong query results). Now an arith /
//! bitwise operand that is a SET column folds to its bitmask; a plain read
//! (or a text compare) keeps the text.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn setup() -> Engine {
    let mut e = mysql();
    e.execute("CREATE TABLE st(id INT, s SET('a','b','c','d'))")
        .unwrap();
    e.execute("INSERT INTO st VALUES (1,'a,c'),(2,'b,d'),(3,'')")
        .unwrap();
    e
}

fn ints(e: &mut Engine, sql: &str) -> Vec<i128> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[0] {
                Value::BigInt(n) => i128::from(*n),
                Value::Int(n) => i128::from(*n),
                Value::Numeric { scaled, scale: 0, .. } => *scaled,
                o => panic!("not int: {o:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn texts(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[0] {
                Value::Text(t) => t.to_string(),
                o => panic!("not text: {o:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

/// `s + 0` is the member bitmask.
#[test]
fn set_plus_zero_is_the_bitmask() {
    let mut e = setup();
    assert_eq!(ints(&mut e, "SELECT s + 0 FROM st ORDER BY id"), vec![5, 10, 0]);
}

/// `s & flag` tests a member.
#[test]
fn set_bitwise_and() {
    let mut e = setup();
    assert_eq!(ints(&mut e, "SELECT s & 2 FROM st ORDER BY id"), vec![0, 2, 0]);
    assert_eq!(ints(&mut e, "SELECT s & 1 FROM st ORDER BY id"), vec![1, 0, 0]);
}

/// `WHERE s & flag` filters by membership (the wrong-results fix).
#[test]
fn where_membership_filter() {
    let mut e = setup();
    assert_eq!(texts(&mut e, "SELECT s FROM st WHERE s & 2 ORDER BY id"), vec!["b,d"]);
    assert_eq!(texts(&mut e, "SELECT s FROM st WHERE s & 1 ORDER BY id"), vec!["a,c"]);
    // bit 8 (member 'd') matches only row 2
    assert_eq!(texts(&mut e, "SELECT s FROM st WHERE s & 8 ORDER BY id"), vec!["b,d"]);
}

/// A plain read (and a text compare) keeps the text — the bitmask is only a
/// numeric-context reading.
#[test]
fn plain_read_keeps_text() {
    let mut e = setup();
    assert_eq!(
        texts(&mut e, "SELECT s FROM st WHERE s = 'a,c'"),
        vec!["a,c"]
    );
    assert_eq!(texts(&mut e, "SELECT s FROM st WHERE id = 2"), vec!["b,d"]);
}
