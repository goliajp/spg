//! v7.39 (round 282) — the `IF EXISTS` NOTICEs that were still missing.
//!
//! Most of the family already raised PG's notice; DROP FUNCTION and DROP
//! TRIGGER did not, and the probe harness did not RENDER notices, so the
//! differential could not see the gap it was meant to find. Fixing the
//! harness came first — that is why this round found anything.
//!
//! Two shapes here are irregular, and both were measured rather than
//! inferred:
//!
//!   * the function notice does NOT quote the name, because it renders a
//!     signature rather than an identifier;
//!   * inside that signature, SQL-standard type KEYWORDS deparse as
//!     `pg_catalog.<internal>` while ordinary identifiers pass through —
//!     so `int` prints `pg_catalog.int4` and the equally valid `int4`
//!     prints `int4`. `date` is NOT a keyword in that production and
//!     prints bare, which is exactly the sort of thing a guessed table
//!     would have got wrong.
//!
//! Every expectation was read off live PG 18.4.

use spg_engine::Engine;

fn notices(e: &mut Engine, sql: &str) -> Vec<String> {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    e.take_notices().into_iter().map(|n| n.message).collect()
}

#[test]
fn a_missing_function_raises_an_unquoted_signature_notice() {
    let mut e = Engine::new();
    assert_eq!(
        notices(&mut e, "DROP FUNCTION IF EXISTS nosuch_f()"),
        vec!["function nosuch_f() does not exist, skipping"],
    );
}

#[test]
fn keyword_types_deparse_schema_qualified_and_identifiers_do_not() {
    let mut e = Engine::new();
    for (sql_type, rendered) in [
        ("int", "pg_catalog.int4"),
        ("integer", "pg_catalog.int4"),
        ("smallint", "pg_catalog.int2"),
        ("bigint", "pg_catalog.int8"),
        ("real", "pg_catalog.float4"),
        ("double precision", "pg_catalog.float8"),
        ("numeric", "pg_catalog.numeric"),
        ("boolean", "pg_catalog.bool"),
        ("character varying", "pg_catalog.varchar"),
        ("character", "pg_catalog.bpchar"),
        ("time", "pg_catalog.time"),
        ("time with time zone", "pg_catalog.timetz"),
        ("timestamp without time zone", "pg_catalog.timestamp"),
        ("timestamp with time zone", "pg_catalog.timestamptz"),
        ("interval", "pg_catalog.interval"),
        ("bit varying", "pg_catalog.varbit"),
        // Not keywords in that grammar production — these stay bare.
        ("text", "text"),
        ("date", "date"),
        ("uuid", "uuid"),
        ("bool", "bool"),
    ] {
        assert_eq!(
            notices(&mut e, &format!("DROP FUNCTION IF EXISTS f1({sql_type})")),
            vec![format!("function f1({rendered}) does not exist, skipping")],
            "{sql_type}",
        );
    }
}

#[test]
fn a_multi_word_type_is_not_mistaken_for_a_parameter_name() {
    // `f(double precision)` is a BARE type; the word count alone reads it
    // as a parameter named `double`. Both spellings must render the same.
    let mut e = Engine::new();
    assert_eq!(
        notices(&mut e, "DROP FUNCTION IF EXISTS f1(double precision)"),
        vec!["function f1(pg_catalog.float8) does not exist, skipping"],
    );
    assert_eq!(
        notices(&mut e, "DROP FUNCTION IF EXISTS f1(x double precision)"),
        vec!["function f1(pg_catalog.float8) does not exist, skipping"],
    );
}

#[test]
fn several_arguments_join_without_a_space() {
    let mut e = Engine::new();
    assert_eq!(
        notices(&mut e, "DROP FUNCTION IF EXISTS f1(int, text)"),
        vec!["function f1(pg_catalog.int4,text) does not exist, skipping"],
    );
}

#[test]
fn drop_trigger_distinguishes_a_missing_relation_from_a_missing_trigger() {
    let mut e = Engine::new();
    // The relation is not there at all — PG cannot even look the trigger
    // up, and says so with the RELATION wording.
    assert_eq!(
        notices(&mut e, "DROP TRIGGER IF EXISTS tg ON nosuch_rel"),
        vec!["relation \"nosuch_rel\" does not exist, skipping"],
    );
    e.execute("CREATE TABLE tgt (a int)").unwrap();
    let _ = e.take_notices();
    assert_eq!(
        notices(&mut e, "DROP TRIGGER IF EXISTS tg ON tgt"),
        vec!["trigger \"tg\" for relation \"tgt\" does not exist, skipping"],
    );
}

#[test]
fn the_plain_forms_still_error_and_raise_nothing() {
    let mut e = Engine::new();
    assert!(e.execute("DROP FUNCTION nosuch_f()").is_err());
    assert!(e.take_notices().is_empty());
    assert!(e.execute("DROP TRIGGER tg ON nosuch_rel").is_err());
    assert!(e.take_notices().is_empty());
}

#[test]
fn a_successful_drop_is_silent() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE tgt (a int)").unwrap();
    let _ = e.take_notices();
    e.execute("DROP FUNCTION IF EXISTS nosuch_f()").unwrap();
    assert_eq!(e.take_notices().len(), 1);
    // …and dropping something that IS there raises nothing.
    e.execute("DROP TABLE IF EXISTS tgt").unwrap();
    assert!(e.take_notices().is_empty());
}
