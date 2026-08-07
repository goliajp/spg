//! v7.39 (round 234) — the jsonb modification family's edge rules. A
//! 85-case sweep of the JSON surface against live PG18.4 (2026-07-19)
//! came back almost entirely clean; what it did find was a cluster of
//! SILENT no-ops where PG raises an error, plus one silently wrong answer:
//!
//!   * `jsonb_set(doc, '{}', v)` — an EMPTY path is a no-op in PG. SPG
//!     replaced the whole document with the new value, so
//!     `jsonb_set('{"a":1}','{}','9')` answered `9` instead of `{"a": 1}`.
//!   * `jsonb_insert(doc, '{}', v)` — also a no-op in PG; SPG raised its
//!     own "path cannot be empty".
//!   * every path-based modification of a SCALAR document (`- key`,
//!     `#- path`, `jsonb_set`, `jsonb_insert`) handed the scalar back
//!     unchanged. `delete_key` ended in a catch-all `(other, _) => other`
//!     arm that swallowed every unsupported combination.
//!   * `'{"a":1}'::jsonb - 0` — an integer index means nothing on an
//!     object; PG says so.
//!
//! All four messages are PG's own, and all are 22023 over the wire.

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
fn empty_path_is_a_no_op() {
    let mut e = Engine::new();
    // PG returns the document untouched rather than replacing it.
    assert_eq!(
        text(&mut e, "SELECT jsonb_set('{\"a\":1}','{}','9')"),
        "{\"a\": 1}"
    );
    assert_eq!(text(&mut e, "SELECT jsonb_set('[1,2]','{}','9')"), "[1, 2]");
    assert_eq!(
        text(&mut e, "SELECT jsonb_set('{\"a\":1}','{}','9',true)"),
        "{\"a\": 1}"
    );
    assert_eq!(
        text(&mut e, "SELECT jsonb_insert('{\"a\":1}','{}','9')"),
        "{\"a\": 1}"
    );
    assert_eq!(
        text(&mut e, "SELECT jsonb_insert('[1,2]','{}','9')"),
        "[1, 2]"
    );
    assert_eq!(text(&mut e, "SELECT '[1,2]'::jsonb #- '{}'"), "[1, 2]");
    // A non-empty path still does its job.
    assert_eq!(
        text(&mut e, "SELECT jsonb_set('{\"a\":1}','{a}','9')"),
        "{\"a\": 9}"
    );
}

#[test]
fn scalar_documents_reject_path_modification() {
    let mut e = Engine::new();
    for (sql, want) in [
        ("SELECT '\"str\"'::jsonb - 'a'", "cannot delete from scalar"),
        ("SELECT '1'::jsonb - 'a'", "cannot delete from scalar"),
        ("SELECT 'true'::jsonb - 'a'", "cannot delete from scalar"),
        ("SELECT 'null'::jsonb - 'a'", "cannot delete from scalar"),
        ("SELECT '\"str\"'::jsonb - 0", "cannot delete from scalar"),
        (
            "SELECT '\"str\"'::jsonb #- '{a}'",
            "cannot delete path in scalar",
        ),
        (
            "SELECT jsonb_set('\"str\"','{a}','9')",
            "cannot set path in scalar",
        ),
        (
            "SELECT jsonb_insert('\"str\"','{a}','9')",
            "cannot set path in scalar",
        ),
    ] {
        let got = err(&mut e, sql);
        assert!(got.contains(want), "{sql}\n  want {want:?}\n  got  {got:?}");
    }
}

#[test]
fn integer_index_is_meaningless_on_an_object() {
    let mut e = Engine::new();
    let got = err(&mut e, "SELECT '{\"a\":1}'::jsonb - 0");
    assert!(
        got.contains("cannot delete from object using integer index"),
        "{got}"
    );
    // The shapes that DO make sense keep working: a key off an object, an
    // index off an array, and a key that isn't there (a no-op in PG).
    assert_eq!(
        text(&mut e, "SELECT '{\"a\":1,\"b\":2}'::jsonb - 'a'"),
        "{\"b\": 2}"
    );
    assert_eq!(text(&mut e, "SELECT '[1,2,3]'::jsonb - 1"), "[1, 3]");
    assert_eq!(
        text(&mut e, "SELECT '{\"a\":1}'::jsonb - 'zz'"),
        "{\"a\": 1}"
    );
    assert_eq!(text(&mut e, "SELECT '[1,2]'::jsonb - 'a'"), "[1, 2]");
}

#[test]
fn the_rest_of_the_modification_family_is_unchanged() {
    let mut e = Engine::new();
    // Regression guard for the sweep's clean cases — these all matched PG
    // before this round and must still.
    for (sql, want) in [
        (
            "SELECT jsonb_set('{\"a\":{\"b\":1}}','{a,b}','9')",
            "{\"a\": {\"b\": 9}}",
        ),
        (
            "SELECT jsonb_set('{\"a\":{\"b\":1}}','{a,c}','9',true)",
            "{\"a\": {\"b\": 1, \"c\": 9}}",
        ),
        (
            "SELECT jsonb_set('{\"a\":{\"b\":1}}','{a,c}','9',false)",
            "{\"a\": {\"b\": 1}}",
        ),
        (
            "SELECT jsonb_insert('{\"a\":[1,2]}','{a,1}','9')",
            "{\"a\": [1, 9, 2]}",
        ),
        (
            "SELECT jsonb_insert('{\"a\":[1,2]}','{a,1}','9',true)",
            "{\"a\": [1, 2, 9]}",
        ),
        (
            "SELECT '{\"a\":{\"b\":1}}'::jsonb #- '{a,b}'",
            "{\"a\": {}}",
        ),
        (
            "SELECT jsonb_strip_nulls('{\"a\":null,\"b\":1}')",
            "{\"b\": 1}",
        ),
        (
            "SELECT '{\"a\":1}'::jsonb || '{\"b\":2}'::jsonb",
            "{\"a\": 1, \"b\": 2}",
        ),
    ] {
        assert_eq!(text(&mut e, sql), want, "{sql}");
    }
}
