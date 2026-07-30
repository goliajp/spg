//! v7.39 (round 636, F35) — four calls that killed the server.
//!
//! This round set out to synthesise `pg_proc`, and the first step was to
//! enumerate what SPG actually implements: probe every function name in the
//! dispatcher and read the engine's own error, which distinguishes
//! "does not exist" from "takes N arg, got 0". The probe stopped at
//! `microsecond` — `SELECT microsecond()` indexed `args[0]` with no arity
//! check and panicked with "the len is 0 but the index is 0", taking the
//! whole spg-server process down, not just the connection.
//!
//! **One crash invalidates the enumeration**: 1149 probes returned 431
//! results, and everything after `microsecond` alphabetically was never
//! reached. So the instrument was rebuilt to survive — probe each function
//! in its own call, restart the server when it dies, keep going — and it
//! found three more:
//!
//!     microsecond()      functions.rs:7649
//!     time_to_sec()      functions.rs:7533
//!     sec_to_time()      functions.rs:7539
//!     obj_description()  functions.rs:17148
//!
//! all the same shape as round 626's `to_char(1,'YYYY')` crash: a
//! user-reachable index out of bounds.
//!
//! Then the question worth answering — how big is the class? The sweep was
//! re-run over all 1148 names at one and two arguments, 2298 probes, and
//! found **none**. The class is exactly these four zero-argument cases.

use spg_engine::{Engine, QueryResult};

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(err) => err.to_string(),
        Ok(ok) => panic!("{sql}: expected a rejection, got {ok:?}"),
    }
}

fn vals(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// The four calls answer rather than abort.
#[test]
fn round636_zero_arg_calls_do_not_panic() {
    let mut e = Engine::new();
    for sql in [
        "SELECT microsecond()",
        "SELECT time_to_sec()",
        "SELECT sec_to_time()",
        "SELECT obj_description()",
    ] {
        let m = err(&mut e, sql);
        assert!(
            m.contains("takes") && m.contains("got 0"),
            "{sql}: wanted an arity error, said {m:?}"
        );
    }
    // The engine is still usable afterwards — the point of the round.
    assert_eq!(vals(&mut e, "SELECT 1+1"), vec!["2"]);
}

/// …and each still does its job when called properly.
#[test]
fn round636_the_four_still_work() {
    let mut e = Engine::new();
    assert_eq!(
        vals(&mut e, "SELECT microsecond(TIME '01:02:03.456789')"),
        vec!["456789"]
    );
    assert_eq!(vals(&mut e, "SELECT time_to_sec(TIME '01:00:01')"), vec!["3601"]);
    assert_eq!(vals(&mut e, "SELECT sec_to_time(3601)"), vec!["01:00:01"]);
    // obj_description over a real relation, and over one with no comment.
    e.execute("CREATE TABLE oc (a INT)").unwrap();
    e.execute("COMMENT ON TABLE oc IS 'a note'").unwrap();
    assert_eq!(
        vals(&mut e, "SELECT obj_description('oc'::regclass, 'pg_class')"),
        vec!["a note"]
    );
    // NULL in, NULL out — the arm the arity guard must not have displaced.
    assert_eq!(
        vals(&mut e, "SELECT microsecond(NULL) IS NULL, time_to_sec(NULL) IS NULL"),
        vec!["true|true"]
    );
}
