//! 9.0.0 — the semantic errors PostgreSQL raises before it scans.
//!
//! PostgreSQL runs parse analysis first, so a statement that cannot
//! mean anything is refused whether or not a row would have reached the
//! expression. SPG checked these at row time, so over an EMPTY table it
//! answered zero rows and no error, and raised the moment the table had
//! one — the same shape as the unknown-column and wrong-arity defects
//! closed before, in three more places.
//!
//! ```text
//!                                        PG 18.6                                  SPG 8.0.4
//!   SELECT CAST(id AS nosuchtype) FROM t type "nosuchtype" does not exist          0 rows
//!   SELECT * FROM t WHERE n              argument of WHERE must be type boolean…   0 rows
//!   SELECT (SELECT s.nosuch FROM t s)    column s.nosuch does not exist            0 rows
//! ```
//!
//! Every expectation is PostgreSQL 18.6's.

use spg_engine::{Engine, QueryResult};

fn boot() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE a9(id int, name text, n numeric)")
        .unwrap();
    e
}

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).expect_err(sql))
}

#[test]
fn a_cast_to_a_type_that_does_not_exist_is_refused_before_the_scan() {
    let mut e = boot();
    for sql in [
        "SELECT CAST(id AS nosuchtype) FROM a9",
        "SELECT id::nosuchtype FROM a9",
        "SELECT * FROM a9 WHERE id::nosuchtype = 1",
        "SELECT * FROM a9 ORDER BY id::nosuchtype",
    ] {
        let m = err(&mut e, sql);
        assert!(
            m.contains("type \"nosuchtype\" does not exist"),
            "{sql}: {m}"
        );
    }
    // The table is EMPTY — that is the whole point — and a type that
    // does exist still casts.
    assert!(matches!(
        e.execute("SELECT id::text FROM a9"),
        Ok(QueryResult::Rows { .. })
    ));
}

#[test]
fn a_predicate_that_is_not_boolean_is_refused_before_the_scan() {
    let mut e = boot();
    let m = err(&mut e, "SELECT * FROM a9 WHERE n");
    assert!(
        m.contains("argument of WHERE must be type boolean, not type numeric"),
        "{m}"
    );
    let m = err(&mut e, "SELECT count(*) FROM a9 HAVING n");
    assert!(
        m.contains("argument of HAVING must be type boolean, not type numeric"),
        "{m}"
    );
    // An untyped literal coerces on PostgreSQL, and a real predicate is
    // untouched.
    assert!(matches!(
        e.execute("SELECT * FROM a9 WHERE id = 1"),
        Ok(QueryResult::Rows { .. })
    ));
    assert!(matches!(
        e.execute("SELECT * FROM a9 WHERE 't'"),
        Ok(QueryResult::Rows { .. })
    ));
}

/// The subquery case is pinned over the WIRE, in
/// `spg-server/tests/e2e/e2e_analysis_time_errors_900.rs`: in-process
/// the engine resolves an uncorrelated scalar subquery by EXECUTING it,
/// and that execution runs the inner statement's own column check — so
/// an engine pin here passes with the analysis pass removed. The read
/// path the server takes does not, which is where the defect lived.
#[test]
fn a_correlated_reference_to_the_outer_scope_still_runs() {
    let mut e = boot();
    assert!(matches!(
        e.execute("SELECT (SELECT o.id FROM a9 s) FROM a9 o"),
        Ok(QueryResult::Rows { .. })
    ));
}
