//! v7.39 (round 244) — sequence-function sweep, 25 cases against live
//! PG18.4 (2026-07-19). nextval/currval/lastval, three-arg setval,
//! negative increments, CYCLE, ALTER SEQUENCE, ::regclass, serial +
//! pg_get_serial_sequence and OWNED BY all matched; the gaps:
//!
//!   * `setval` ACCEPTED A VALUE OUTSIDE THE SEQUENCE'S RANGE silently,
//!     leaving last_value out of bounds — PG's 22003 "setval: value 99
//!     is out of bounds for sequence \"sq3\" (1..6)";
//!   * a missing sequence reported "corrupt on-disk format: sequence …"
//!     — a user mistake dressed as data corruption; it is PG's plain
//!     `relation "x" does not exist` (42P01);
//!   * `SELECT … FROM <sequence>` — PG exposes every sequence as a
//!     one-row relation (last_value, log_cnt, is_called), which psql's
//!     \d and several ORMs read; SPG said the relation didn't exist;
//!   * the CREATE SEQUENCE START refusal takes PG's two named wordings
//!     (below MINVALUE / above MAXVALUE, 22023).

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => rows[0]
            .values
            .iter()
            .map(spg_engine::eval::value_to_text)
            .collect::<Vec<_>>()
            .join("|"),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(x) => format!("{x}"),
        Ok(ok) => panic!("{sql}: expected an error, got {ok:?}"),
    }
}

#[test]
fn setval_rejects_out_of_range_values()  {
    let mut e = Engine::new();
    e.execute("CREATE SEQUENCE sq3 MAXVALUE 6").unwrap();
    let got = err(&mut e, "SELECT setval('sq3', 99)");
    assert!(
        got.contains("setval: value 99 is out of bounds for sequence \"sq3\" (1..6)"),
        "{got}"
    );
    // In-range setval still works and drives the next nextval.
    assert_eq!(one(&mut e, "SELECT setval('sq3', 5)"), "5");
    assert_eq!(one(&mut e, "SELECT nextval('sq3')"), "6");
}

#[test]
fn missing_sequence_is_a_missing_relation() {
    let mut e = Engine::new();
    for sql in ["SELECT nextval('nosuch')", "SELECT currval('nosuch')"] {
        let got = err(&mut e, sql);
        assert!(got.contains("relation \"nosuch\" does not exist"), "{sql}: {got}");
        assert!(!got.contains("corrupt"), "user error dressed as corruption: {got}");
    }
}

#[test]
fn a_sequence_is_a_one_row_relation() {
    let mut e = Engine::new();
    e.execute("CREATE SEQUENCE sq").unwrap();
    e.execute("SELECT nextval('sq')").unwrap();
    e.execute("SELECT nextval('sq')").unwrap();
    assert_eq!(one(&mut e, "SELECT last_value FROM sq"), "2");
    assert_eq!(
        one(&mut e, "SELECT last_value, log_cnt, is_called FROM sq"),
        "2|0|true"
    );
    // Fresh sequence: last_value = start, is_called = false.
    e.execute("CREATE SEQUENCE sqf START 7").unwrap();
    assert_eq!(one(&mut e, "SELECT last_value, is_called FROM sqf"), "7|false");
}

#[test]
fn create_sequence_start_refusals_take_pgs_wording() {
    let mut e = Engine::new();
    let got = err(&mut e, "CREATE SEQUENCE bad1 MINVALUE 5 START 3");
    assert!(
        got.contains("START value (3) cannot be less than MINVALUE (5)"),
        "{got}"
    );
    let got = err(&mut e, "CREATE SEQUENCE bad2 MAXVALUE 5 START 9");
    assert!(
        got.contains("START value (9) cannot be greater than MAXVALUE (5)"),
        "{got}"
    );
}

#[test]
fn the_sequence_core_is_unchanged() {
    let mut e = Engine::new();
    // Regression guard over the sweep's clean cases.
    e.execute("CREATE SEQUENCE sq").unwrap();
    assert_eq!(one(&mut e, "SELECT nextval('sq'), nextval('sq'), currval('sq')"), "1|2|2");
    assert_eq!(one(&mut e, "SELECT lastval()"), "2");
    assert_eq!(one(&mut e, "SELECT setval('sq', 200, false)"), "200");
    assert_eq!(one(&mut e, "SELECT nextval('sq')"), "200");
    e.execute("CREATE SEQUENCE sq2 INCREMENT -2 START 10 MINVALUE 0 MAXVALUE 10")
        .unwrap();
    assert_eq!(one(&mut e, "SELECT nextval('sq2'), nextval('sq2')"), "10|8");
    e.execute("CREATE SEQUENCE sq4 CYCLE MAXVALUE 2").unwrap();
    assert_eq!(
        one(&mut e, "SELECT nextval('sq4'), nextval('sq4'), nextval('sq4')"),
        "1|2|1"
    );
}
