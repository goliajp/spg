//! read01 round 344 (V49) — the SQL type spellings have one owner.
//!
//! `is_multiword_type_phrase` existed THREE times: twice in spg-sql (the
//! round-282 copy took no length modifier) and once, byte-identical, in
//! spg-storage — the two crates being siblings that did not depend on
//! each other. spg-sql is a dependency-free leaf, so spg-storage can just
//! depend on it; the publish order already put spg-sql first, which is
//! the same reason the direction is the natural one.
//!
//! Nothing observable changes from the dedup itself. What DID change is a
//! wall the third copy was hiding: a length / precision modifier on a
//! function parameter. PG 18.4 accepts `f(character varying(9))` and
//! `f(numeric(10,2))` and DROPS the modifier — `pg_get_function_arguments`
//! reports plain `character varying` / `numeric` — while SPG raised
//! `syntax error at or near "("`.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => match rows.first().and_then(|r| r.values.first()) {
            Some(spg_storage::Value::Text(t)) => t.to_string(),
            other => panic!("{sql}: {other:?}"),
        },
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

/// One list, reachable from both crates, still answering the same way.
#[test]
fn both_crates_read_the_same_list() {
    for phrase in [
        "double precision",
        "character varying",
        "timestamp with time zone",
        "national character varying",
        "bit varying",
    ] {
        assert!(
            spg_sql::parser::is_multiword_type_phrase(phrase),
            "{phrase}"
        );
        assert!(spg_storage::is_multiword_type_phrase(phrase), "{phrase}");
    }
    for phrase in ["integer", "x integer", "time", "text"] {
        assert!(
            !spg_sql::parser::is_multiword_type_phrase(phrase),
            "{phrase}"
        );
        assert!(!spg_storage::is_multiword_type_phrase(phrase), "{phrase}");
    }
    // A modifier is peeled before the match — the copy that did not peel
    // is what this round removed.
    assert!(spg_sql::parser::is_multiword_type_phrase(
        "character varying(9)"
    ));
}

/// The wall the third copy hid: PG takes a modifier on a parameter type.
#[test]
fn a_parameter_type_may_carry_a_modifier() {
    let mut e = Engine::new();
    e.execute("CREATE FUNCTION m1(character varying(9)) RETURNS int LANGUAGE sql AS 'SELECT 1'")
        .expect("PG accepts a length modifier on a parameter");
    e.execute("CREATE FUNCTION m2(numeric(10,2)) RETURNS int LANGUAGE sql AS 'SELECT 1'")
        .expect("…and a precision one");
    // PG drops it: pg_get_function_arguments reports the bare type.
    assert!(
        text(&mut e, "SELECT pg_get_functiondef('m1'::regproc)").contains("m1(character varying)"),
        "the modifier is not retained",
    );
    assert!(text(&mut e, "SELECT pg_get_functiondef('m2'::regproc)").contains("m2(numeric)"),);
}

/// The distinction the list exists for is untouched: a leading word that
/// starts a multi-word type is part of the TYPE, not a parameter name.
#[test]
fn a_bare_multiword_type_is_still_not_a_parameter_name() {
    let mut e = Engine::new();
    e.execute("CREATE FUNCTION m3(double precision) RETURNS int LANGUAGE sql AS 'SELECT 1'")
        .unwrap();
    e.execute("CREATE FUNCTION m4(x double precision) RETURNS int LANGUAGE sql AS 'SELECT 1'")
        .unwrap();
    assert!(
        text(&mut e, "SELECT pg_get_functiondef('m3'::regproc)").contains("m3(double precision)"),
    );
    assert!(
        text(&mut e, "SELECT pg_get_functiondef('m4'::regproc)").contains("m4(x double precision)"),
    );
}
