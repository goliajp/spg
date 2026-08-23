//! v7.38.19 — a function resolved at STATEMENT level keeps its column
//! name.
//!
//! `nextval`, `setval`, the `pg_advisory_*` family and the `lo_*` family
//! are evaluated by a pre-pass that REPLACES the call node with the
//! literal it produced: they mutate engine state, and the value dispatch
//! only ever holds a shared reference. A literal has no name to figure
//! out, so every one of them came back as `?column?` where PostgreSQL
//! 18.4 answers the function's own name. Measured, both engines, same
//! statements:
//!
//! ```text
//!   SELECT nextval('s')                    PG: nextval           SPG: ?column?
//!   SELECT setval('s', 5)                  PG: setval            SPG: ?column?
//!   SELECT pg_advisory_lock(1)             PG: pg_advisory_lock  SPG: ?column?
//!   SELECT pg_advisory_unlock(1)           PG: pg_advisory_unlock SPG: ?column?
//!   SELECT lo_from_bytea(0, 'ab'::bytea)   PG: lo_from_bytea     SPG: ?column?
//! ```
//!
//! An ORM or a driver that reads column names got `?column?` for all of
//! them.
//!
//! Found while measuring a "recorded delta" comment which said
//! `pg_advisory_unlock` differs from PG only in a missing WARNING. It
//! differs in the name too, and so does the whole family the comment
//! never mentioned.
//!
//! **This is an engine test and not a corpus file**, for the same
//! measured reason as `e2e_udf_result_type`: the corpus runner has no
//! sequence resolver, so `nextval` errors there before a name could be
//! compared, and it never asks for column names anyway.

use spg_engine::{Engine, QueryResult};

fn names(e: &mut Engine, sql: &str) -> Vec<String> {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { columns, .. } => columns.iter().map(|c| c.name.clone()).collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn a_statement_resolved_call_is_named_after_its_function() {
    let mut e = Engine::new();
    e.execute("CREATE SEQUENCE sq").unwrap();

    assert_eq!(names(&mut e, "SELECT nextval('sq')"), ["nextval"]);
    assert_eq!(names(&mut e, "SELECT setval('sq', 5)"), ["setval"]);
    assert_eq!(
        names(&mut e, "SELECT pg_advisory_lock(1)"),
        ["pg_advisory_lock"]
    );
    assert_eq!(
        names(&mut e, "SELECT pg_advisory_unlock(1)"),
        ["pg_advisory_unlock"]
    );
    assert_eq!(
        names(&mut e, "SELECT pg_try_advisory_lock(2)"),
        ["pg_try_advisory_lock"]
    );
    assert_eq!(
        names(&mut e, "SELECT lo_from_bytea(0, 'ab'::bytea)"),
        ["lo_from_bytea"]
    );
}

/// The controls, each measured on PG 18.4 too. An ordinary function was
/// always named; an expression with no name of its own is `?column?` on
/// both engines; and an explicit alias still wins.
#[test]
fn the_names_that_were_already_right_are_unchanged() {
    let mut e = Engine::new();
    e.execute("CREATE SEQUENCE sq2").unwrap();

    assert_eq!(names(&mut e, "SELECT abs(-1)"), ["abs"]);
    assert_eq!(names(&mut e, "SELECT 1+1"), ["?column?"]);
    assert_eq!(names(&mut e, "SELECT nextval('sq2') AS mine"), ["mine"]);
    // Two items, one resolved and one not: naming one must not disturb
    // the other or their order.
    assert_eq!(
        names(&mut e, "SELECT nextval('sq2'), abs(-2)"),
        ["nextval", "abs"]
    );
}
