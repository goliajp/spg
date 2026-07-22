//! read01 round 342 (V65) — regproc carries its oid, like regclass.
//!
//! Round 339 taught `::regproc` to resolve a user function, but SPG
//! carried the result as plain TEXT. Two things followed:
//!
//!   * `pg_proc.oid = 'f'::regproc` — the join the reference itself
//!     exists for — could not resolve, because there was no oid half;
//!   * a callee could not tell `pg_get_functiondef('f'::regproc)`, which
//!     PG answers, from `pg_get_functiondef('f')`, which PG rejects with
//!     `invalid input syntax for type oid: "f"`. SPG answered both.
//!
//! `Value::RegProc(oid, name)` mirrors `Value::RegClass` exactly — same
//! dual shape, same comparison rules, same "eval-only, never persisted"
//! contract (the codec writes it as absent, so no FILE_VERSION change).
//!
//! PG 18.4 measured: `pg_typeof(to_regproc('f'))` is `regproc` and
//! `pg_typeof(to_regprocedure('f(integer)'))` is `regprocedure`.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn first(e: &mut Engine, sql: &str) -> Value<'static> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
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

/// The join the reference exists for. It could not resolve at all.
#[test]
fn pg_proc_joins_on_a_regproc() {
    let mut e = fixture();
    assert_eq!(
        first(&mut e, "SELECT proname FROM pg_proc WHERE oid = 'ff'::regproc"),
        Value::text("ff"),
    );
    // …and by signature, which is what regprocedure carries.
    assert_eq!(
        first(
            &mut e,
            "SELECT proname FROM pg_proc WHERE oid = 'g(int,text)'::regprocedure"
        ),
        Value::text("g"),
    );
    // to_regproc answers the same shape, so it joins too.
    assert_eq!(
        first(
            &mut e,
            "SELECT proname FROM pg_proc WHERE oid = to_regproc('ff')"
        ),
        Value::text("ff"),
    );
}

/// Both halves are readable, as with regclass.
#[test]
fn it_renders_as_the_name_and_casts_to_the_oid() {
    let mut e = fixture();
    assert!(matches!(
        first(&mut e, "SELECT 'ff'::regproc"),
        Value::RegProc(_, _)
    ));
    assert_eq!(first(&mut e, "SELECT 'ff'::regproc::text"), Value::text("ff"));
    assert_eq!(
        first(&mut e, "SELECT 'ff'::regproc::oid > 0"),
        Value::Bool(true)
    );
    assert_eq!(
        first(&mut e, "SELECT 'g(int,text)'::regprocedure::text"),
        Value::text("g(integer,text)"),
    );
    assert_eq!(
        first(&mut e, "SELECT 'ff'::regproc = 'ff'::regproc"),
        Value::Bool(true)
    );
}

/// One carrier, two PG types — told apart by the argument list.
#[test]
fn pg_typeof_tells_regproc_from_regprocedure() {
    let mut e = fixture();
    assert_eq!(
        first(&mut e, "SELECT pg_typeof('ff'::regproc)"),
        Value::text("regproc")
    );
    assert_eq!(
        first(&mut e, "SELECT pg_typeof(to_regproc('ff'))"),
        Value::text("regproc")
    );
    assert_eq!(
        first(&mut e, "SELECT pg_typeof(to_regprocedure('g(integer,text)'))"),
        Value::text("regprocedure"),
    );
}

/// The distinction the TEXT carrier could not make.
#[test]
fn a_regproc_and_a_bare_string_are_different_arguments() {
    let mut e = fixture();
    let def = first(&mut e, "SELECT pg_get_functiondef('ff'::regproc)");
    let Value::Text(t) = &def else { panic!("{def:?}") };
    assert!(t.contains("CREATE OR REPLACE FUNCTION public.ff"), "{t}");
    // PG 18.4 on the bare string, verbatim.
    assert_eq!(
        err(&mut e, "SELECT pg_get_functiondef('ff')"),
        "eval: type mismatch: invalid input syntax for type oid: \"ff\"",
    );
    // The oid form every existing caller uses is untouched.
    assert_eq!(
        first(
            &mut e,
            "SELECT pg_get_functiondef(oid) FROM pg_proc WHERE proname = 'ff'"
        ),
        def,
    );
}

/// A miss still answers the way each spelling promises.
#[test]
fn misses_keep_their_shapes() {
    let mut e = fixture();
    assert_eq!(first(&mut e, "SELECT to_regproc('nosuch')"), Value::Null);
    assert_eq!(
        err(&mut e, "SELECT 'nosuch'::regproc"),
        "eval: type mismatch: function \"nosuch\" does not exist",
    );
}
