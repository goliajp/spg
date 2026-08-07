//! read01 round 339 (V63) — `::regproc` names a user function.
//!
//! Name resolution ran against the static pg_proc table alone, so
//! `'my_fn'::regproc` raised `function "my_fn" does not exist` for a
//! function that plainly did — and that is the form catalog queries and
//! pg_dump use to name one. `to_regproc` answered NULL for it,
//! `to_regprocedure` answered NULL for everything, and
//! `pg_get_functiondef('f'::regproc)` never got past the cast.
//!
//! PG 18.4 measured: `'g(int,text)'::regprocedure` renders
//! `g(integer,text)` — canonical type names, no space after the comma —
//! and a signature that matches nothing echoes the input verbatim:
//! `function "g339(text,int)" does not exist`.

use spg_engine::Engine;
use spg_storage::Value;

fn first(e: &mut Engine, sql: &str) -> Value<'static> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        spg_engine::QueryResult::Rows { rows, .. } => rows
            .first()
            .and_then(|r| r.values.first())
            .cloned()
            .map(Value::into_owned)
            .unwrap_or(Value::Null),
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(v) => panic!("{sql}: expected an error, got {v:?}"),
        Err(x) => format!("{x}"),
    }
}

fn fixture() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE FUNCTION ff(a int) RETURNS int LANGUAGE sql AS 'SELECT a + 1'")
        .unwrap();
    e.execute("CREATE FUNCTION g(a int, b text) RETURNS int LANGUAGE sql AS 'SELECT a'")
        .unwrap();
    e
}

#[test]
fn regproc_resolves_a_user_function() {
    let mut e = fixture();
    // v7.39 (round 342, V65) — a regproc carries its oid now; the
    // rendering is still the name.
    assert!(matches!(
        first(&mut e, "SELECT 'ff'::regproc"),
        Value::RegProc(_, _)
    ));
    assert_eq!(
        first(&mut e, "SELECT 'ff'::regproc::text"),
        Value::text("ff")
    );
    // A name that is no function is still PG's error…
    assert_eq!(
        err(&mut e, "SELECT 'nosuchfn'::regproc"),
        "eval: type mismatch: function \"nosuchfn\" does not exist",
    );
    // …and an overloaded built-in still reports the ambiguity.
    assert!(err(&mut e, "SELECT 'lower'::regproc").contains("more than one function named"),);
}

/// regprocedure carries the argument list, so an overload IS
/// distinguishable — and the rendering is PG's canonical spelling.
#[test]
fn regprocedure_renders_the_canonical_signature() {
    let mut e = fixture();
    assert_eq!(
        first(&mut e, "SELECT 'ff(int)'::regprocedure::text"),
        Value::text("ff(integer)"),
    );
    assert_eq!(
        first(&mut e, "SELECT 'g(int,text)'::regprocedure::text"),
        Value::text("g(integer,text)"),
    );
    // Argument order is part of the signature.
    assert_eq!(
        err(&mut e, "SELECT 'g(text,int)'::regprocedure"),
        "eval: type mismatch: function \"g(text,int)\" does not exist",
    );
}

/// The to_reg* spellings answer NULL for a miss rather than erroring —
/// that difference is why PG has both.
#[test]
fn to_regproc_and_to_regprocedure_answer_for_user_functions() {
    let mut e = fixture();
    // v7.39 (round 342, V65) — the reg* shape, read through ::text.
    assert_eq!(
        first(&mut e, "SELECT to_regproc('ff')::text"),
        Value::text("ff")
    );
    assert_eq!(
        first(&mut e, "SELECT to_regproc('g')::text"),
        Value::text("g")
    );
    assert_eq!(first(&mut e, "SELECT to_regproc('nosuchfn')"), Value::Null);
    assert_eq!(
        first(&mut e, "SELECT to_regprocedure('ff(integer)')::text"),
        Value::text("ff(integer)"),
    );
    assert_eq!(
        first(&mut e, "SELECT to_regprocedure('ff(text)')"),
        Value::Null
    );
}

/// The whole point of resolving the cast: the function's definition
/// through the spelling PG's own documentation uses.
#[test]
fn pg_get_functiondef_takes_a_regproc() {
    let mut e = fixture();
    let by_regproc = first(&mut e, "SELECT pg_get_functiondef('ff'::regproc)");
    let Value::Text(def) = &by_regproc else {
        panic!("{by_regproc:?}");
    };
    assert!(
        def.contains("CREATE OR REPLACE FUNCTION public.ff(a integer)"),
        "{def}"
    );
    // The oid form — what every existing caller uses — is unchanged.
    assert_eq!(
        first(
            &mut e,
            "SELECT pg_get_functiondef(oid) FROM pg_proc WHERE proname = 'ff'"
        ),
        by_regproc,
    );
    assert_eq!(
        first(&mut e, "SELECT pg_get_functiondef(999999)"),
        Value::Null
    );
}
