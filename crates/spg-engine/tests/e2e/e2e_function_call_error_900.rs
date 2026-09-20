//! 9.0.0 — a call no overload accepts says what PostgreSQL says.
//!
//! The evaluator wrote its own sentence — `lower() needs text, got
//! integer` — at 183 sites, plus three more spellings (`requires`,
//! `expects`) elsewhere. PostgreSQL 18.6 answers `function
//! lower(integer) does not exist` for every one of them, and names a
//! bare string literal `unknown`, not `text`:
//!
//! ```text
//!   nosuchfn('a','b')   function nosuchfn(unknown, unknown) does not exist
//!   strpos(1,'a')       function strpos(integer, unknown) does not exist
//!   date_trunc('day',1) function date_trunc(unknown, integer) does not exist
//! ```
//!
//! The rewrite sits at the call boundary, where the call's own
//! expressions are in hand — the same place two earlier rounds already
//! rename operands PostgreSQL names differently. It fires only when
//! every argument can be typed: a rewrite that guessed at one would
//! trade a clumsy sentence for a false one.
//!
//! `unnest`'s refusal has THREE copies (`select.rs` twice,
//! `table_access.rs` once). Ablating the `table_access.rs` one changes
//! nothing in the whole e2e suite — 7,237 tests — so no shape the
//! corpus carries reaches it. Recorded, not deleted: an ablation that
//! does not bite says the pin cannot reach the code, not that the code
//! is dead.
//!
//! NOT closed, measured: a call whose ONE argument is a bare string
//! literal. PostgreSQL commits the literal to the only candidate's
//! type and reports the input function's error — `abs('x')` is
//! `invalid input syntax for type double precision: "x"` — which needs
//! a per-function candidate table. Those keep the evaluator's own
//! sentence rather than a newly wrong one.

use spg_engine::{Engine, QueryResult};

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(err) => err.to_string(),
        Ok(ok) => panic!("{sql}: expected an error, got {ok:?}"),
    }
}

#[test]
fn every_spelling_becomes_pgs_sentence() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id int, name text)").unwrap();
    e.execute("INSERT INTO t VALUES (1,'a')").unwrap();
    for (sql, want) in [
        // `needs`
        (
            "SELECT lower(id) FROM t",
            "function lower(integer) does not exist",
        ),
        ("SELECT upper(1)", "function upper(integer) does not exist"),
        (
            "SELECT length(1)",
            "function length(integer) does not exist",
        ),
        // `requires`
        (
            "SELECT bool_and(1)",
            "function bool_and(integer) does not exist",
        ),
        (
            "SELECT bool_or(1)",
            "function bool_or(integer) does not exist",
        ),
        // `expects`
        (
            "SELECT unnest(1)",
            "function unnest(integer) does not exist",
        ),
        (
            "SELECT * FROM unnest(1)",
            "function unnest(integer) does not exist",
        ),
        // and one the argument expression cannot be typed through, so
        // the message's own tail supplies it.
        (
            "SELECT lower(ARRAY[1,2])",
            "function lower(integer[]) does not exist",
        ),
    ] {
        let got = err(&mut e, sql);
        assert!(got.contains(want), "{sql}\n  got:  {got}\n  want: {want}");
    }
}

#[test]
fn a_bare_string_literal_is_unknown_not_text() {
    let mut e = Engine::new();
    for (sql, want) in [
        (
            "SELECT nosuchfn('a','b')",
            "function nosuchfn(unknown, unknown) does not exist",
        ),
        (
            "SELECT strpos(1,'a')",
            "function strpos(integer, unknown) does not exist",
        ),
        (
            "SELECT date_trunc('day', 1)",
            "function date_trunc(unknown, integer) does not exist",
        ),
        (
            "SELECT concat_ws(0,'a','b')",
            "function concat_ws(integer, unknown, unknown) does not exist",
        ),
    ] {
        let got = err(&mut e, sql);
        assert!(got.contains(want), "{sql}\n  got:  {got}\n  want: {want}");
    }
    // A cast says `text`, and PostgreSQL agrees.
    assert!(
        err(&mut e, "SELECT strpos(1,'a'::text)")
            .contains("function strpos(integer, text) does not exist"),
        "{}",
        err(&mut e, "SELECT strpos(1,'a'::text)")
    );
}

#[test]
fn a_lone_unknown_literal_is_committed_to_the_candidate() {
    // 9.0.0 — this pin RECORDED the divergence; it asserts PostgreSQL's
    // answer now. PG commits a lone unknown literal to the only
    // candidate's type and reports the input function's error, and for
    // the numeric family that type is `double precision`, the preferred
    // type of the category. Measured on 18.6 for all five.
    let mut e = Engine::new();
    for sql in [
        "SELECT abs('x')",
        "SELECT sqrt('x')",
        "SELECT ceil('x')",
        "SELECT ln('x')",
        "SELECT round('x')",
    ] {
        let got = err(&mut e, sql);
        assert!(
            got.contains("invalid input syntax for type double precision: \"x\""),
            "{sql}: {got}"
        );
    }
    // A TYPED argument is a different question, and it already matched.
    let got = err(&mut e, "SELECT abs('x'::text)");
    assert!(got.contains("function abs(text) does not exist"), "{got}");
}

#[test]
fn an_unknown_literal_beside_a_typed_operand_reports_the_input_error() {
    // Measured on PG 18.6: `1 + 'x'`, `'x' + 1`, `1 - 'x'` answer
    // `invalid input syntax for type integer: "x"` and `1.5 + 'x'`
    // answers the numeric one. The comparison spelling `1 = 'x'` already
    // matched, and must keep matching.
    let mut e = Engine::new();
    for (sql, ty) in [
        ("SELECT 1 + 'x'", "integer"),
        ("SELECT 'x' + 1", "integer"),
        ("SELECT 1 - 'x'", "integer"),
        ("SELECT 1.5 + 'x'", "numeric"),
        ("SELECT 1 = 'x'", "integer"),
    ] {
        let got = err(&mut e, sql);
        assert!(
            got.contains(&format!("invalid input syntax for type {ty}: \"x\"")),
            "{sql}: {got}"
        );
    }
    // And the shapes that ANSWER keep answering.
    e.execute("SELECT 'a' || 1").expect("concat");
    e.execute("SELECT 1 + 2").expect("addition");
}

#[test]
fn the_calls_that_work_still_work() {
    let mut e = Engine::new();
    let QueryResult::Rows { rows, .. } = e.execute("SELECT lower('A'), upper('b')").unwrap() else {
        panic!("expected rows");
    };
    assert_eq!(
        rows[0]
            .values
            .iter()
            .map(spg_engine::eval::value_to_text)
            .collect::<Vec<_>>(),
        vec!["a".to_string(), "B".to_string()]
    );
}
