//! read01 round 322 (V46) — `CREATE FUNCTION` attribute clauses.
//!
//! `CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql IMMUTABLE STRICT AS
//! $$…$$` was a **parse error**: "expected AS before function body, got
//! Ident(\"immutable\")". PG emits exactly that shape from `pg_dump`, so a
//! dump of any function carrying a volatility or strictness clause did not
//! restore.
//!
//! Every expectation below was read off live PG 18.4:
//!
//!   * the clauses are accepted on either side of the body, in any order;
//!   * `pg_get_functiondef` prints them on their own line between LANGUAGE
//!     and AS, ordered volatility, PARALLEL, STRICT, SECURITY DEFINER,
//!     LEAKPROOF, COST, ROWS — and prints no such line when everything is
//!     at its default;
//!   * `STRICT` is semantic: `f(NULL)` is NULL and the body never runs
//!     (a strict `SELECT coalesce(a,-1)` answers NULL, not -1);
//!   * `pg_proc` reports provolatile / proisstrict / prosecdef /
//!     proleakproof / proparallel / procost / prorows.

use spg_engine::Engine;
use spg_storage::Value;

fn scalar(e: &mut Engine, sql: &str) -> Value<'static> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}")) {
        spg_engine::QueryResult::Rows { rows, .. } => rows
            .first()
            .and_then(|r| r.values.first())
            .cloned()
            .map(spg_storage::Value::into_owned)
            .unwrap_or_else(|| panic!("no cell for `{sql}`")),
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

fn text(e: &mut Engine, sql: &str) -> String {
    match scalar(e, sql) {
        Value::Text(t) => t.to_string(),
        other => panic!("`{sql}` did not return text: {other:?}"),
    }
}

/// The shape pg_dump emits. It used to fail to parse.
#[test]
fn a_function_may_declare_volatility_and_strictness() {
    let mut e = Engine::new();
    e.execute(
        "CREATE FUNCTION f1(a int) RETURNS int LANGUAGE sql IMMUTABLE STRICT AS $$ SELECT a + 1 $$",
    )
    .expect("IMMUTABLE STRICT must parse");
    assert_eq!(scalar(&mut e, "SELECT f1(1)"), Value::Int(2));
}

/// PG accepts the clauses after the body too.
#[test]
fn attributes_are_accepted_on_either_side_of_the_body() {
    let mut e = Engine::new();
    e.execute("CREATE FUNCTION f2() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$ IMMUTABLE")
        .expect("trailing IMMUTABLE must parse");
    let def = text(&mut e, "SELECT pg_get_functiondef(oid) FROM pg_proc WHERE proname = 'f2'");
    assert!(
        def.contains("\n IMMUTABLE\n"),
        "the attribute line must be there: {def}"
    );
}

/// STRICT is not decoration: a NULL argument short-circuits the body.
#[test]
fn strict_returns_null_without_running_the_body() {
    let mut e = Engine::new();
    e.execute(
        "CREATE FUNCTION s1(a int) RETURNS int LANGUAGE sql STRICT AS $$ SELECT coalesce(a, -1) $$",
    )
    .unwrap();
    e.execute(
        "CREATE FUNCTION s2(a int) RETURNS int LANGUAGE sql AS $$ SELECT coalesce(a, -1) $$",
    )
    .unwrap();

    assert_eq!(
        scalar(&mut e, "SELECT s1(NULL)"),
        Value::Null,
        "a strict function short-circuits"
    );
    assert_eq!(scalar(&mut e, "SELECT s1(5)"), Value::Int(5));
    assert_eq!(
        scalar(&mut e, "SELECT s2(NULL)"),
        Value::Int(-1),
        "a non-strict function still runs its body"
    );
}

/// `RETURNS NULL ON NULL INPUT` and `CALLED ON NULL INPUT` are the
/// spelled-out forms of STRICT and its opposite.
#[test]
fn the_spelled_out_null_input_clauses_work() {
    let mut e = Engine::new();
    e.execute(
        "CREATE FUNCTION n1(a int) RETURNS int LANGUAGE sql RETURNS NULL ON NULL INPUT \
         AS $$ SELECT coalesce(a, -1) $$",
    )
    .unwrap();
    e.execute(
        "CREATE FUNCTION n2(a int) RETURNS int LANGUAGE sql CALLED ON NULL INPUT \
         AS $$ SELECT coalesce(a, -1) $$",
    )
    .unwrap();
    assert_eq!(scalar(&mut e, "SELECT n1(NULL)"), Value::Null);
    assert_eq!(scalar(&mut e, "SELECT n2(NULL)"), Value::Int(-1));
}

/// `pg_get_functiondef` prints PG's exact attribute line and ordering.
#[test]
fn functiondef_prints_the_attribute_line_in_pgs_order() {
    let mut e = Engine::new();
    e.execute(
        "CREATE FUNCTION f4() RETURNS int LANGUAGE sql \
         IMMUTABLE STRICT LEAKPROOF SECURITY DEFINER PARALLEL SAFE COST 7 AS $$ SELECT 1 $$",
    )
    .unwrap();
    let def = text(&mut e, "SELECT pg_get_functiondef(oid) FROM pg_proc WHERE proname = 'f4'");
    assert!(
        def.contains("\n IMMUTABLE PARALLEL SAFE STRICT SECURITY DEFINER LEAKPROOF COST 7\n"),
        "PG's order is volatility, PARALLEL, STRICT, SECURITY DEFINER, \
         LEAKPROOF, COST, ROWS: {def}"
    );

    // An all-default function has no attribute line at all.
    e.execute("CREATE FUNCTION f5() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$")
        .unwrap();
    let plain = text(&mut e, "SELECT pg_get_functiondef(oid) FROM pg_proc WHERE proname = 'f5'");
    assert!(
        plain.contains(" LANGUAGE sql\nAS $function$"),
        "no attribute line when nothing was declared: {plain}"
    );
}

/// `ROWS` rides along for set-returning functions.
#[test]
fn rows_is_recorded_and_printed() {
    let mut e = Engine::new();
    e.execute("CREATE FUNCTION f6() RETURNS SETOF int LANGUAGE sql STABLE ROWS 5 AS $$ SELECT 1 $$")
        .unwrap();
    let def = text(&mut e, "SELECT pg_get_functiondef(oid) FROM pg_proc WHERE proname = 'f6'");
    assert!(def.contains("\n STABLE ROWS 5\n"), "{def}");
}

/// `pg_proc` reports what was declared, not a row of defaults.
#[test]
fn pg_proc_reports_the_declared_attributes() {
    let mut e = Engine::new();
    e.execute(
        "CREATE FUNCTION p1() RETURNS int LANGUAGE sql \
         STABLE STRICT SECURITY DEFINER LEAKPROOF PARALLEL RESTRICTED COST 42 AS $$ SELECT 1 $$",
    )
    .unwrap();
    assert_eq!(
        text(&mut e, "SELECT provolatile FROM pg_proc WHERE proname = 'p1'"),
        "s"
    );
    assert_eq!(
        text(&mut e, "SELECT proparallel FROM pg_proc WHERE proname = 'p1'"),
        "r"
    );
    assert_eq!(
        scalar(&mut e, "SELECT proisstrict FROM pg_proc WHERE proname = 'p1'"),
        Value::Bool(true)
    );
    assert_eq!(
        scalar(&mut e, "SELECT prosecdef FROM pg_proc WHERE proname = 'p1'"),
        Value::Bool(true)
    );
    assert_eq!(
        scalar(&mut e, "SELECT proleakproof FROM pg_proc WHERE proname = 'p1'"),
        Value::Bool(true)
    );
    assert_eq!(
        scalar(&mut e, "SELECT procost FROM pg_proc WHERE proname = 'p1'"),
        Value::Float(42.0)
    );

    // And a function that declared nothing still reads as PG's defaults.
    e.execute("CREATE FUNCTION p2() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$")
        .unwrap();
    assert_eq!(
        text(&mut e, "SELECT provolatile FROM pg_proc WHERE proname = 'p2'"),
        "v"
    );
    assert_eq!(
        scalar(&mut e, "SELECT proisstrict FROM pg_proc WHERE proname = 'p2'"),
        Value::Bool(false)
    );
    assert_eq!(
        scalar(&mut e, "SELECT procost FROM pg_proc WHERE proname = 'p2'"),
        Value::Float(100.0)
    );
}

/// The attributes survive a snapshot round-trip (FILE_VERSION 80 block).
#[test]
fn attributes_survive_a_reload() {
    let mut e = Engine::new();
    e.execute(
        "CREATE FUNCTION r1(a int) RETURNS int LANGUAGE sql IMMUTABLE STRICT COST 3 \
         AS $$ SELECT coalesce(a, -1) $$",
    )
    .unwrap();
    let bytes = e.catalog().serialize();
    let mut back = Engine::restore_envelope(&bytes).expect("reload");

    assert_eq!(
        scalar(&mut back, "SELECT r1(NULL)"),
        Value::Null,
        "STRICT must survive the round-trip"
    );
    let def = text(&mut back, "SELECT pg_get_functiondef(oid) FROM pg_proc WHERE proname = 'r1'");
    assert!(def.contains("\n IMMUTABLE STRICT COST 3\n"), "{def}");
}
