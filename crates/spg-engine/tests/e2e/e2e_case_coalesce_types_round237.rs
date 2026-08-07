//! v7.39 (round 237) — CASE / COALESCE / GREATEST / LEAST resolve their
//! branches to one type, finishing what round 233 (set operations) and
//! round 236 (ARRAY constructors) started. SPG returned whichever branch
//! fired, so `CASE WHEN true THEN 1 ELSE 'a'::text END` answered `1` and
//! the very same expression answered text on a row where the other branch
//! won — the type of a column depended on the data in it.
//!
//! Checked STATICALLY, unlike the ARRAY path: CASE runs only the branch it
//! takes, and a COALESCE argument may have side effects
//! (`COALESCE(nextval('s'), 1)`), so evaluating every branch to inspect it
//! would change what the query does.
//!
//! The branch ORDER is observable and was probed, not assumed: PG resolves
//! the ELSE branch first, so `THEN 1 ELSE 'a'::text` reports "text and
//! integer" while a two-WHEN `THEN 1 ... THEN true` reports "integer and
//! boolean".

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Null => "NULL".to_string(),
            v => spg_engine::eval::value_to_text(v),
        },
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(x) => format!("{x}"),
        Ok(ok) => panic!("{sql}: expected an error, got {ok:?}"),
    }
}

#[test]
fn case_branches_must_share_a_type() {
    let mut e = Engine::new();
    for (sql, want) in [
        (
            "SELECT CASE WHEN true THEN 1 ELSE 'a'::text END",
            "CASE types text and integer cannot be matched",
        ),
        (
            "SELECT CASE WHEN true THEN 'a'::text ELSE 1 END",
            "CASE types integer and text cannot be matched",
        ),
        (
            "SELECT CASE WHEN true THEN 1 WHEN false THEN true END",
            "CASE types integer and boolean cannot be matched",
        ),
    ] {
        let got = err(&mut e, sql);
        assert_eq!(got, format!("eval: type mismatch: {want}"), "{sql}");
    }
    // An untyped literal adopts the other branches' type; a value that will
    // not convert is reported as itself.
    assert_eq!(
        text(&mut e, "SELECT CASE WHEN true THEN 1 ELSE '2' END"),
        "1"
    );
    let got = err(&mut e, "SELECT CASE WHEN true THEN 1 ELSE 'a' END");
    assert!(
        got.contains("invalid input syntax for type integer: \"a\""),
        "{got}"
    );
    let got = err(&mut e, "SELECT CASE 1 WHEN 1 THEN 'a' ELSE 2 END");
    assert!(
        got.contains("invalid input syntax for type integer: \"a\""),
        "{got}"
    );
    // Same-family branches still resolve, and a CASE with no ELSE is NULL.
    assert_eq!(
        text(&mut e, "SELECT CASE WHEN true THEN 1 ELSE 2.5 END"),
        "1"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT pg_typeof(CASE WHEN true THEN 1 ELSE 2.5 END)::text"
        ),
        "numeric"
    );
    assert_eq!(text(&mut e, "SELECT CASE WHEN false THEN 1 END"), "NULL");
}

#[test]
fn coalesce_greatest_and_least_share_the_rule() {
    let mut e = Engine::new();
    for (sql, want) in [
        (
            "SELECT coalesce(1,'a'::text)",
            "COALESCE types integer and text cannot be matched",
        ),
        (
            "SELECT coalesce(1,true)",
            "COALESCE types integer and boolean cannot be matched",
        ),
        (
            "SELECT greatest(1,'a'::text)",
            "GREATEST types integer and text cannot be matched",
        ),
        (
            "SELECT greatest(1,true)",
            "GREATEST types integer and boolean cannot be matched",
        ),
        (
            "SELECT least(1,'a'::text)",
            "LEAST types integer and text cannot be matched",
        ),
    ] {
        let got = err(&mut e, sql);
        assert_eq!(got, format!("eval: type mismatch: {want}"), "{sql}");
    }
    let got = err(&mut e, "SELECT coalesce(1,'a')");
    assert!(
        got.contains("invalid input syntax for type integer: \"a\""),
        "{got}"
    );
    // The working shapes are untouched.
    assert_eq!(text(&mut e, "SELECT coalesce(1,'2')"), "1");
    assert_eq!(
        text(&mut e, "SELECT pg_typeof(coalesce(1,'2'))::text"),
        "integer"
    );
    assert_eq!(text(&mut e, "SELECT coalesce(NULL,1)"), "1");
    assert_eq!(
        text(&mut e, "SELECT pg_typeof(coalesce(1,2.5))::text"),
        "numeric"
    );
    assert_eq!(text(&mut e, "SELECT greatest('a','b')"), "b");
    assert_eq!(
        text(&mut e, "SELECT pg_typeof(least(1,2.5))::text"),
        "numeric"
    );
}

#[test]
fn only_confidently_typed_branches_are_judged() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ev (id int, payload jsonb)")
        .unwrap();
    e.execute("INSERT INTO ev VALUES (1,'{\"user\":{\"h\":{\"a\":\"b\"}}}')")
        .unwrap();
    // A general expression's type is a best-effort hint, not a contract:
    // `describe_expr` reports a binary operator as its LEFT operand's type,
    // so `payload->'user'` came back as text and an earlier version of this
    // check refused this working query. Refusing a valid query is worse
    // than missing an invalid one, so inferred branches are left alone.
    assert_eq!(
        text(
            &mut e,
            "SELECT coalesce(payload->'user'->'h', '{}'::jsonb)::text FROM ev"
        ),
        "{\"a\": \"b\"}"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT CASE WHEN true THEN payload->'user' ELSE '{}'::jsonb END IS NOT NULL FROM ev"
        ),
        "true"
    );
}
