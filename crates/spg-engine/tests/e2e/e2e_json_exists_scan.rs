//! v7.38.19 — `?`, `?|` and `?&` stopped building a document to answer
//! whether one key is present.
//!
//! Measured on 25,000 rows of an 885-byte document carrying 41 keys:
//!
//!     d ? 'target'            SPG 246.591 ms      PG 18   2.291
//!
//! and on sentori's own four-key `traits`, 200,000 rows:
//!
//!     traits ? 'plan'         SPG  44.000 ms      PG 18   6.097
//!     traits->>'plan'         SPG  14.137
//!
//! The last line is the one that named it. `->>` finds the same key AND
//! copies the value out, and cost a third of what merely locating cost
//! -- because the accessor has walked bytes since v7.38.9 while the
//! existence operators called `parse`, allocating every key and every
//! value of the whole document to answer a yes/no about one of them.
//!
//! The expectations below are PostgreSQL 18.4's, taken from it directly
//! rather than reasoned about: every case was run against the oracle
//! and against this engine before the change, and all 27 agreed. They
//! are here so they go on agreeing.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(t) => t.to_string(),
            spg_storage::Value::Bool(b) => b.to_string(),
            spg_storage::Value::Null => "<NULL>".into(),
            other => format!("{other:?}"),
        },
        other => panic!("expected rows from {sql}, got {other:?}"),
    }
}

/// Every row here was answered by PostgreSQL 18.4 before it was written
/// down. The interesting ones are the four that are FALSE where a
/// careless scanner says true: a key that is only a value, a key nested
/// one level down, a number that prints like the key, and an object
/// inside an array.
#[test]
fn the_existence_operator_matches_postgresql() {
    let mut e = Engine::new();
    for (case, sql, want) in [
        ("obj-hit", r#"SELECT '{"a":1,"b":2}'::jsonb ? 'a'"#, "true"),
        (
            "obj-miss",
            r#"SELECT '{"a":1,"b":2}'::jsonb ? 'c'"#,
            "false",
        ),
        ("obj-empty", r#"SELECT '{}'::jsonb ? 'a'"#, "false"),
        (
            "value-not-key",
            r#"SELECT '{"a":"b"}'::jsonb ? 'b'"#,
            "false",
        ),
        (
            "nested-key",
            r#"SELECT '{"a":{"b":1}}'::jsonb ? 'b'"#,
            "false",
        ),
        ("arr-string", r#"SELECT '["a","b"]'::jsonb ? 'a'"#, "true"),
        ("arr-number", r#"SELECT '[1,2]'::jsonb ? '1'"#, "false"),
        (
            "arr-nested-obj",
            r#"SELECT '[{"a":1}]'::jsonb ? 'a'"#,
            "false",
        ),
        ("arr-empty", r#"SELECT '[]'::jsonb ? 'a'"#, "false"),
        ("bare-string", r#"SELECT '"a"'::jsonb ? 'a'"#, "true"),
        ("bare-string-no", r#"SELECT '"a"'::jsonb ? 'b'"#, "false"),
        ("bare-number", r#"SELECT '1'::jsonb ? '1'"#, "false"),
        ("bare-null", r#"SELECT 'null'::jsonb ? 'null'"#, "false"),
        ("bare-true", r#"SELECT 'true'::jsonb ? 'true'"#, "false"),
        (
            "escaped-key",
            r#"SELECT '{"a\"b":1}'::jsonb ? 'a"b'"#,
            "true",
        ),
        (
            "whitespace",
            r#"SELECT '{ "a" : 1 , "b" : 2 }'::jsonb ? 'b'"#,
            "true",
        ),
        ("empty-key", r#"SELECT '{"":1}'::jsonb ? ''"#, "true"),
        ("null-doc", "SELECT NULL::jsonb ? 'a'", "<NULL>"),
        ("null-key", r#"SELECT '{"a":1}'::jsonb ? NULL"#, "<NULL>"),
        (
            "any-hit",
            r#"SELECT '{"a":1}'::jsonb ?| ARRAY['x','a']"#,
            "true",
        ),
        (
            "any-miss",
            r#"SELECT '{"a":1}'::jsonb ?| ARRAY['x','y']"#,
            "false",
        ),
        (
            "any-empty",
            r#"SELECT '{"a":1}'::jsonb ?| ARRAY[]::text[]"#,
            "false",
        ),
        (
            "all-hit",
            r#"SELECT '{"a":1,"b":2}'::jsonb ?& ARRAY['a','b']"#,
            "true",
        ),
        (
            "all-miss",
            r#"SELECT '{"a":1}'::jsonb ?& ARRAY['a','b']"#,
            "false",
        ),
        (
            "all-empty",
            r#"SELECT '{"a":1}'::jsonb ?& ARRAY[]::text[]"#,
            "true",
        ),
    ] {
        assert_eq!(one(&mut e, sql), want, "{case}: {sql}");
    }
}

/// PostgreSQL has no `?` for the `json` type at all -- it answers
/// `operator does not exist`. We accept it, which is looser than PG and
/// predates this change; these pin that the looser path still answers
/// the way it did. A `json` document keeps its duplicates and its
/// insertion order, and existence does not care about either.
#[test]
fn the_looser_json_spelling_still_answers() {
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, r#"SELECT '{"zz":1,"a":2,"zz":3}'::json ? 'zz'"#),
        "true"
    );
    assert_eq!(
        one(&mut e, r#"SELECT '{"zz":1,"a":2}'::json ? 'a'"#),
        "true"
    );
    assert_eq!(
        one(&mut e, r#"SELECT '{"zz":1,"a":2}'::json ? 'b'"#),
        "false"
    );
}

/// The document is walked, not parsed -- but a document that came from
/// a TEXT literal was never canonicalised, so it is still validated
/// before being trusted. Invalid JSON must not answer `false`; it must
/// say so.
#[test]
fn invalid_json_on_the_left_is_still_an_error() {
    let mut e = Engine::new();
    let err = e.execute(r#"SELECT '{"a":1'::text ? 'a'"#);
    assert!(
        err.is_err(),
        "an unparseable document must not quietly answer false: {err:?}"
    );
}

/// A key found while walking must be found at the TOP level of a
/// document with structure around it -- a scanner that lost its place
/// inside a nested value would either miss the later keys or find keys
/// belonging to the nesting.
#[test]
fn nesting_does_not_hide_or_invent_top_level_keys() {
    let mut e = Engine::new();
    let doc = r#"'{"a":{"x":1,"y":[1,2,{"z":3}]},"b":[{"c":4}],"d":"}","e":1}'::jsonb"#;
    for (key, want) in [
        ("a", "true"),
        ("b", "true"),
        ("d", "true"),
        ("e", "true"),
        ("x", "false"),
        ("y", "false"),
        ("z", "false"),
        ("c", "false"),
    ] {
        assert_eq!(
            one(&mut e, &format!("SELECT {doc} ? '{key}'")),
            want,
            "key {key} at the top level of {doc}"
        );
    }
}

/// `?|` and `?&` walk the document ONCE, marking every key asked about,
/// rather than once per key. On sentori's `traits`, 200,000 rows,
/// `?| ARRAY['x','plan']` cost 41.891 ms against PostgreSQL 18's 7.546
/// where a single `?` cost 9.749 against 6.763 — two keys, two walks.
///
/// Every expectation is PostgreSQL 18.4's, taken from it. The ones that
/// matter to a single-pass marker are the duplicates (a key asked about
/// twice must not decrement the outstanding count twice and stop the
/// walk early), the misses that must survive to the end of the
/// document, and `arr-mixed`, where a nested object holds the second
/// key and must not satisfy it.
#[test]
fn the_array_forms_match_postgresql() {
    let mut e = Engine::new();
    for (case, sql, want) in [
        (
            "any-both",
            r#"SELECT '{"a":1,"b":2}'::jsonb ?| ARRAY['a','b']"#,
            "true",
        ),
        (
            "any-second",
            r#"SELECT '{"a":1,"b":2}'::jsonb ?| ARRAY['z','b']"#,
            "true",
        ),
        (
            "any-dup",
            r#"SELECT '{"a":1}'::jsonb ?| ARRAY['a','a']"#,
            "true",
        ),
        (
            "all-dup",
            r#"SELECT '{"a":1}'::jsonb ?& ARRAY['a','a']"#,
            "true",
        ),
        (
            "all-three",
            r#"SELECT '{"a":1,"b":2,"c":3}'::jsonb ?& ARRAY['c','a','b']"#,
            "true",
        ),
        (
            "all-one-miss",
            r#"SELECT '{"a":1,"b":2,"c":3}'::jsonb ?& ARRAY['c','a','z']"#,
            "false",
        ),
        (
            "arr-any",
            r#"SELECT '["a","b"]'::jsonb ?| ARRAY['z','b']"#,
            "true",
        ),
        (
            "arr-all",
            r#"SELECT '["a","b"]'::jsonb ?& ARRAY['a','b']"#,
            "true",
        ),
        (
            "arr-all-miss",
            r#"SELECT '["a","b"]'::jsonb ?& ARRAY['a','z']"#,
            "false",
        ),
        (
            "arr-mixed",
            r#"SELECT '["a",1,{"b":2}]'::jsonb ?& ARRAY['a','b']"#,
            "false",
        ),
        (
            "str-any",
            r#"SELECT '"a"'::jsonb ?| ARRAY['a','z']"#,
            "true",
        ),
        (
            "str-all",
            r#"SELECT '"a"'::jsonb ?& ARRAY['a','z']"#,
            "false",
        ),
        ("num-any", r#"SELECT '1'::jsonb ?| ARRAY['1']"#, "false"),
        (
            "nested-any",
            r#"SELECT '{"a":{"b":1}}'::jsonb ?| ARRAY['b','z']"#,
            "false",
        ),
        (
            "esc-any",
            r#"SELECT '{"a\"b":1}'::jsonb ?| ARRAY['a"b','z']"#,
            "true",
        ),
        (
            "empty-doc-all",
            r#"SELECT '{}'::jsonb ?& ARRAY['a']"#,
            "false",
        ),
        (
            "empty-doc-any",
            r#"SELECT '{}'::jsonb ?| ARRAY['a']"#,
            "false",
        ),
    ] {
        assert_eq!(one(&mut e, sql), want, "{case}: {sql}");
    }
}

/// The hazard a single-pass marker has and a per-key walk does not: one
/// document key matching an entry that is ALREADY marked. Drop the
/// "not already found" guard and the second `zz` counts a second time,
/// the walk believes every key is accounted for, and it stops before
/// reaching `q`.
///
/// It takes a `json` document to build: `jsonb` collapses duplicate
/// keys on the way in, and PostgreSQL has no `?&` for `json` at all, so
/// this expectation is the operator's own meaning rather than the
/// oracle's answer — both keys are present, so both are present.
///
/// The first version of this test used `'{"a":1}' ?& ARRAY['a','a']`,
/// which passes with the guard removed. Written, run against the
/// deliberately broken scanner, and found not to bite; the ORDER is
/// what makes it bite — the duplicate has to come before the key that
/// is still outstanding.
#[test]
fn a_repeated_document_key_does_not_end_the_walk_early() {
    let mut e = Engine::new();
    assert_eq!(
        one(
            &mut e,
            r#"SELECT '{"zz":1,"zz":3,"q":2}'::json ?& ARRAY['zz','q']"#
        ),
        "true"
    );
    assert_eq!(
        one(
            &mut e,
            r#"SELECT '{"zz":1,"zz":3,"q":2}'::json ?& ARRAY['zz','nope']"#
        ),
        "false"
    );
}

/// A NULL element of the key array names no key, and PostgreSQL 18.4
/// ignores it rather than failing to find it. `?& ARRAY['a',NULL]` is
/// TRUE, and so is `?& ARRAY[NULL]` — vacuously, there being nothing
/// left to look for.
///
/// The code this replaced expressed that by dropping NULLs while
/// copying the array. Borrowing the array instead meant the positions
/// stayed, and the first version of the borrow answered FALSE for both
/// of these — it counted a NULL as a key it had failed to find.
#[test]
fn a_null_element_names_no_key() {
    let mut e = Engine::new();
    for (sql, want) in [
        (r#"SELECT '{"a":1}'::jsonb ?& ARRAY['a',NULL]"#, "true"),
        (r#"SELECT '{"a":1}'::jsonb ?| ARRAY['a',NULL]"#, "true"),
        (
            r#"SELECT '{"a":1}'::jsonb ?| ARRAY[NULL,NULL]::text[]"#,
            "false",
        ),
        (r#"SELECT '{"a":1}'::jsonb ?& ARRAY[NULL]::text[]"#, "true"),
        (
            r#"SELECT '{"a":1}'::jsonb ?& ARRAY[NULL,'z']::text[]"#,
            "false",
        ),
        (
            r#"SELECT '{"a":1}'::jsonb ?| ARRAY[NULL,'a']::text[]"#,
            "true",
        ),
    ] {
        assert_eq!(one(&mut e, sql), want, "{sql}");
    }
}
