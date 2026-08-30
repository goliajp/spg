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
