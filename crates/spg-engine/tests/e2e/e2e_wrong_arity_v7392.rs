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
//! v7.39.3 finishes the sweep: the families that still wrote their own
//! arithmetic — inet, encode/decode, the text-search calls, trim, the
//! window functions, `nextval`, the aggregates — all answer the one
//! sentence now. The aggregate check runs BEFORE evaluation, over
//! unevaluated expressions, so it takes the argument types from the
//! lexeme and the schema (`unknown` for a bare literal, the declared
//! type for a column, both measured); a call whose argument has no
//! static type to name keeps the old sentence rather than inventing
//! one.
//!
//! Only the COUNT raises this. A right-count call with wrong types
//! gets the same sentence from PostgreSQL but a different errno from
//! MySQL (1582 is `Incorrect parameter count`, which would be a lie),
//! so those keep whatever their own guard gives.

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
    // v7.39.3 — and the second over-refusal, which the workspace suite
    // caught before this pin existed. `json_insert` / `json_set` are
    // routed by the VALUE of their second argument — a `$`-path picks
    // MySQL's implementation, anything else picks PostgreSQL's, which
    // takes 3 or 4 arguments. The table's probe called them with NULLs,
    // never reached the MySQL arm, and recorded five arguments as
    // refused. Five is ordinary MySQL.
    assert!(
        e.execute("SELECT JSON_INSERT('{\"a\": 1}', '$.a', 99, '$.b', 2) FROM vt")
            .is_ok(),
        "a value-routed variadic call must survive the pre-scan check"
    );
    assert!(
        e.execute("SELECT JSON_SET('{\"a\": 1}', '$.a', 10, '$.c', 2) FROM vt")
            .is_ok()
    );
    assert!(
        e.execute("SELECT JSON_ARRAY_APPEND('[1]', '$', 2, '$', 3) FROM vt")
            .is_ok()
    );
}

/// v7.39.3 — the families that were still writing their own arithmetic.
///
/// Every `want` below is the sentence PostgreSQL 18.6 gives for that
/// exact SQL, measured against the oracle container, not derived from
/// SPG's own output.
#[test]
fn the_remaining_families_say_it_too() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE fam (t TEXT, n INT)").expect("ddl");
    e.execute("INSERT INTO fam VALUES ('a', 1)")
        .expect("insert");
    for (sql, want) in [
        // inet
        ("SELECT host()", "function host() does not exist"),
        (
            "SELECT host('a','b')",
            "function host(unknown, unknown) does not exist",
        ),
        (
            "SELECT masklen(t, n) FROM fam",
            "function masklen(text, integer) does not exist",
        ),
        // encode / decode
        (
            "SELECT encode('a')",
            "function encode(unknown) does not exist",
        ),
        (
            "SELECT decode('a')",
            "function decode(unknown) does not exist",
        ),
        // trim family
        (
            "SELECT ltrim('a','b','c')",
            "function ltrim(unknown, unknown, unknown) does not exist",
        ),
        // arrays
        (
            "SELECT string_to_array('a')",
            "function string_to_array(unknown) does not exist",
        ),
        // full-text search
        (
            "SELECT to_tsvector('a','b','c')",
            "function to_tsvector(unknown, unknown, unknown) does not exist",
        ),
        (
            "SELECT setweight('a')",
            "function setweight(unknown) does not exist",
        ),
        // formatting
        (
            "SELECT to_char(n) FROM fam",
            "function to_char(integer) does not exist",
        ),
    ] {
        assert_eq!(err(&mut e, sql), want, "{sql}");
    }
}

/// The two shapes that reach it through a different door: a window
/// function (validated while the partition is built) and an aggregate
/// (validated over the unevaluated statement, before any row work).
#[test]
fn a_window_and_an_aggregate_say_it_too() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE wf (a INT, t TEXT)").expect("ddl");
    e.execute("INSERT INTO wf VALUES (1, 'x')").expect("insert");
    for (sql, want) in [
        (
            "SELECT lag() OVER () FROM wf",
            "function lag() does not exist",
        ),
        (
            "SELECT first_value() OVER () FROM wf",
            "function first_value() does not exist",
        ),
        // The aggregate check names a bare literal `unknown` and a
        // column by its declared type, the way PostgreSQL does.
        (
            "SELECT count(1, 2) FROM wf",
            "function count(integer, integer) does not exist",
        ),
        (
            "SELECT sum(a, t) FROM wf",
            "function sum(integer, text) does not exist",
        ),
        (
            "SELECT max('a', 'b') FROM wf",
            "function max(unknown, unknown) does not exist",
        ),
        ("SELECT nextval()", "function nextval() does not exist"),
    ] {
        assert_eq!(err(&mut e, sql), want, "{sql}");
    }
    // Right arity still runs — the guard must not have swallowed them.
    assert!(e.execute("SELECT lag(a) OVER () FROM wf").is_ok());
    assert!(e.execute("SELECT count(a) FROM wf").is_ok());
    assert!(e.execute("SELECT sum(a) FROM wf").is_ok());
}

/// The control for the aggregate path: an argument with no static type
/// keeps the older sentence rather than being given an invented one.
#[test]
fn an_aggregate_argument_with_no_static_type_is_not_invented() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ag (a INT)").expect("ddl");
    let msg = err(&mut e, "SELECT count(a + 1, a * 2) FROM ag");
    assert!(
        !msg.contains("unknown"),
        "an expression must not be called `unknown`: {msg}"
    );
    assert!(msg.contains("count("), "{msg}");
}
