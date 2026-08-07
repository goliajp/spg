//! v7.39 (round 235) — jsonpath's two evaluation modes, the gap round 234
//! scoped out. The leading `strict` / `lax` word was parsed and THROWN
//! AWAY ("SPG evaluates lax semantics; the mode word is accepted and
//! stripped"), so `strict` was silently a no-op and lax itself was
//! incomplete. Both halves are now real, measured against live PG18.4
//! (2026-07-19):
//!
//!   * STRICT reports what lax skips: a missing object key, an
//!     out-of-bounds subscript, a wildcard on a non-array, a member
//!     accessor on a non-object.
//!   * LAX auto-UNWRAPS an array for a member accessor and auto-WRAPS a
//!     non-array for an array accessor — `lax $.a` over `[{"a":1}]` and
//!     `lax $[*]` over `1` both used to return nothing.
//!   * `jsonb_path_match`, `@?` and `@@` SUPPRESS a strict refusal and
//!     answer NULL, while `jsonb_path_query`, `_query_first`,
//!     `_query_array` and `_exists` raise it.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => {
            if rows.is_empty() {
                return "<none>".to_string();
            }
            match &rows[0].values[0] {
                spg_storage::Value::Null => "NULL".to_string(),
                v => spg_engine::eval::value_to_text(v),
            }
        }
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
fn strict_mode_reports_what_lax_skips() {
    let mut e = Engine::new();
    for (sql, want) in [
        (
            "SELECT jsonb_path_query('{\"a\":1}','strict $.b')",
            "JSON object does not contain key \"b\"",
        ),
        (
            "SELECT jsonb_path_query('{\"a\":{\"b\":1}}','strict $.a.c')",
            "JSON object does not contain key \"c\"",
        ),
        (
            "SELECT jsonb_path_query('[1,2]','strict $[5]')",
            "jsonpath array subscript is out of bounds",
        ),
        (
            "SELECT jsonb_path_query_array('[1,2]','strict $[0 to 5]')",
            "jsonpath array subscript is out of bounds",
        ),
        (
            "SELECT jsonb_path_query('1','strict $[*]')",
            "jsonpath wildcard array accessor can only be applied to an array",
        ),
        (
            "SELECT jsonb_path_query('{\"a\":1}','strict $[*]')",
            "jsonpath wildcard array accessor can only be applied to an array",
        ),
        (
            "SELECT jsonb_path_query('[1,[2]]','strict $[*][*]')",
            "jsonpath wildcard array accessor can only be applied to an array",
        ),
        (
            "SELECT jsonb_path_query('1','strict $.a')",
            "jsonpath member accessor can only be applied to an object",
        ),
        (
            "SELECT jsonb_path_query('[{\"a\":1}]','strict $.a')",
            "jsonpath member accessor can only be applied to an object",
        ),
    ] {
        let got = err(&mut e, sql);
        assert!(got.contains(want), "{sql}\n  want {want:?}\n  got  {got:?}");
    }
    // Strict paths that DO resolve still answer, and a filter that matches
    // nothing is empty rather than an error in either mode.
    assert_eq!(
        one(
            &mut e,
            "SELECT jsonb_path_query('{\"a\":{\"b\":1}}','strict $.a.b')"
        ),
        "1"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT jsonb_path_query('[[1,2]]','strict $[*][*]')"
        ),
        "1"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT jsonb_path_query('{\"a\":[1,2]}','strict $.a[*] ? (@ > 5)')"
        ),
        "<none>"
    );
}

#[test]
fn lax_mode_wraps_and_unwraps() {
    let mut e = Engine::new();
    // A member accessor looks inside an array.
    assert_eq!(
        one(&mut e, "SELECT jsonb_path_query('[{\"a\":1}]','lax $.a')"),
        "1"
    );
    // An array accessor treats a non-array as one element.
    assert_eq!(one(&mut e, "SELECT jsonb_path_query('1','lax $[*]')"), "1");
    assert_eq!(
        one(&mut e, "SELECT jsonb_path_query('{\"a\":1}','lax $[*]')"),
        "{\"a\": 1}"
    );
    assert_eq!(one(&mut e, "SELECT jsonb_path_query('1','lax $[0]')"), "1");
    // Past the end of that one-element array there is nothing — no error.
    assert_eq!(
        one(&mut e, "SELECT jsonb_path_query('1','lax $[1]')"),
        "<none>"
    );
    // The misses strict complains about are simply empty in lax.
    assert_eq!(
        one(&mut e, "SELECT jsonb_path_query('{\"a\":1}','lax $.b')"),
        "<none>"
    );
    assert_eq!(
        one(&mut e, "SELECT jsonb_path_query('[1,2]','lax $[5]')"),
        "<none>"
    );
    // Lax is the default when no mode word is given.
    assert_eq!(
        one(&mut e, "SELECT jsonb_path_query('[{\"a\":1}]','$.a')"),
        "1"
    );
}

#[test]
fn only_the_error_suppressing_forms_answer_null() {
    let mut e = Engine::new();
    // Suppress → NULL.
    assert_eq!(
        one(
            &mut e,
            "SELECT jsonb_path_match('{\"a\":1}','strict $.b == 1')"
        ),
        "NULL"
    );
    assert_eq!(
        one(&mut e, "SELECT '{\"a\":1}'::jsonb @? 'strict $.b'"),
        "NULL"
    );
    assert_eq!(
        one(&mut e, "SELECT '{\"a\":1}'::jsonb @@ 'strict $.b == 1'"),
        "NULL"
    );
    // Raise.
    for sql in [
        "SELECT jsonb_path_exists('{\"a\":1}','strict $.b')",
        "SELECT jsonb_path_query_first('{\"a\":1}','strict $.b')",
        "SELECT jsonb_path_query_array('{\"a\":1}','strict $.b')",
    ] {
        let got = err(&mut e, sql);
        assert!(
            got.contains("JSON object does not contain key"),
            "{sql}: {got}"
        );
    }
    // The lax forms of the same calls keep their old answers.
    assert_eq!(
        one(&mut e, "SELECT jsonb_path_exists('{\"a\":1}','$.b')"),
        "false"
    );
    assert_eq!(
        one(&mut e, "SELECT jsonb_path_exists('{\"a\":1}','$.a')"),
        "true"
    );
}
