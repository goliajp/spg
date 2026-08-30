//! v7.39.2 — what each engine says when a built-in is called with the
//! wrong number of arguments.
//!
//! SPG wrote its own arithmetic at 339 sites across ten files —
//! `lower() takes 1 arg, got 0` — and neither engine says that. Measured:
//!
//!   PostgreSQL 18.6  `function lower() does not exist`
//!                    `function lower(text, integer) does not exist`
//!   MySQL 9.7.2      `Incorrect parameter count in the call to native
//!                     function 'LOWER'`, errno 1582
//!
//! PostgreSQL draws no line between a missing FUNCTION and a missing
//! OVERLOAD — `nosuchfn(text, integer, date)` gets the identical
//! sentence — so its rendering is the engine's canonical one and both
//! come from one place. MySQL does draw that line and numbers the two
//! differently (1582 against 1305), which is why this is its own error
//! variant rather than a formatted string the wire would have to guess
//! the meaning of.
//!
//! Two sites keep their own wording, and the reason is not taste: the
//! `setval` and aggregate arity checks run BEFORE evaluation, over
//! unevaluated expressions, so they cannot name the argument types
//! PostgreSQL's sentence carries.

use spg_engine::Engine;

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).unwrap_err())
        .trim_start_matches("eval: ")
        .trim_start_matches("type mismatch: ")
        .to_string()
}

#[test]
fn postgresql_names_the_signature_it_could_not_match() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ar (t TEXT, n INT, f FLOAT)")
        .unwrap();
    e.execute("INSERT INTO ar VALUES ('a', 1, 1.5)").unwrap();
    for (sql, want) in [
        ("SELECT lower()", "function lower() does not exist"),
        (
            "SELECT lower(t, n) FROM ar",
            "function lower(text, integer) does not exist",
        ),
        (
            "SELECT substr(t) FROM ar",
            "function substr(text) does not exist",
        ),
        (
            "SELECT round(f, n, t) FROM ar",
            "function round(double precision, integer, text) does not exist",
        ),
        // The four that once took the whole process down (round 636).
        (
            "SELECT microsecond()",
            "function microsecond() does not exist",
        ),
        (
            "SELECT time_to_sec()",
            "function time_to_sec() does not exist",
        ),
        (
            "SELECT sec_to_time()",
            "function sec_to_time() does not exist",
        ),
        (
            "SELECT obj_description()",
            "function obj_description() does not exist",
        ),
    ] {
        assert_eq!(err(&mut e, sql), want, "{sql}");
    }
    // Right arity still runs.
    assert!(e.execute("SELECT lower(t) FROM ar").is_ok());
}

// MySQL's own sentence is rendered at the WIRE (it carries an errno
// PostgreSQL has no equivalent of), so it is pinned in the server suite:
// `e2e_mysqlwire_query::wrong_arity_counts_the_parameters`.

/// v7.39.2 — a wrong-arity call is refused BEFORE the scan, so an empty
/// table stops hiding it.
///
/// `SELECT lower(t, n) FROM t` answered zero rows and no error while
/// `t` was empty and raised the moment it had one row in it, because
/// the arity check lives inside the row-time dispatch. Same shape as
/// the unknown-column-in-a-predicate defect, in a different place: a
/// query written against an empty fixture passed its test.
///
/// The accepted counts come from `eval::arity`, derived by asking the
/// dispatch itself offline — see that file for the two oracles that
/// were tried and refuted, and for the over-refusal the first version
/// of the probe caused.
#[test]
fn an_empty_table_stops_hiding_a_wrong_arity_call() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE et (t TEXT, n INT)").expect("ddl");
    // Empty, and it is refused — with PostgreSQL's own signature.
    assert_eq!(
        err(&mut e, "SELECT lower(t, n) FROM et"),
        "function lower(text, integer) does not exist"
    );
    assert_eq!(
        err(&mut e, "SELECT substr(t) FROM et"),
        "function substr(text) does not exist"
    );
    // The same answer once there IS a row — the point is that the two
    // agree, which is what they did not do before.
    e.execute("INSERT INTO et VALUES ('a', 1)").expect("insert");
    assert_eq!(
        err(&mut e, "SELECT lower(t, n) FROM et"),
        "function lower(text, integer) does not exist"
    );
}

/// The control: a variadic call is NOT refused early. The first version
/// of the probe asked one dialect and the table was consulted in both,
/// so this exact call came back rejected before the scan.
#[test]
fn a_variadic_call_is_not_refused_early() {
    let mut e = Engine::new();
    e.set_mysql_dialect(true);
    e.execute("CREATE TABLE vt (a INT)").expect("ddl");
    assert!(
        e.execute("SELECT JSON_OBJECT('k', 1, 'v', 2) FROM vt")
            .is_ok(),
        "a variadic call must survive the pre-scan check"
    );
    assert!(e.execute("SELECT CONCAT('a','b','c') FROM vt").is_ok());
    assert!(e.execute("SELECT COALESCE(a, 1, 2) FROM vt").is_ok());
}
