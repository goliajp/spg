//! v7.39 (round 236) — ARRAY constructor element-type resolution.
//!
//! Sweeps of the array surface (45 cases) and the string-function surface
//! (45 cases) against live PG18.4 (2026-07-19) came back essentially
//! clean — the strings were 45/45. What they exposed was the SAME hole
//! round 233 closed for set operations, sitting in the expression
//! constructors: PG resolves an ARRAY constructor's elements to one
//! element type and refuses the constructor when they have no common one,
//! while SPG DEGRADED TO `text[]`. `ARRAY[1, true]` came back as `{1,t}`
//! — a column of rendered strings that then behaved like text everywhere
//! downstream — and `ARRAY[1, 'a']` as `{1,a}` where PG reports the value
//! that would not convert.
//!
//! The untyped-literal subtlety is the same one as round 233: a bare
//! string or NULL literal is PG's `unknown` and adopts the other elements'
//! type, so it is identified from the SYNTAX, not from the runtime value.

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
fn elements_with_no_common_type_are_refused() {
    let mut e = Engine::new();
    for (sql, want) in [
        ("SELECT ARRAY[1, true]", "ARRAY types integer and boolean cannot be matched"),
        (
            "SELECT ARRAY[1, 'a'::text]",
            "ARRAY types integer and text cannot be matched",
        ),
        (
            "SELECT ARRAY['a'::text, 1]",
            "ARRAY types text and integer cannot be matched",
        ),
        // PG names the real array type here, not information_schema's
        // `ARRAY` pseudo-name.
        (
            "SELECT ARRAY[1, ARRAY[2]]",
            "ARRAY types integer and integer[] cannot be matched",
        ),
    ] {
        let got = err(&mut e, sql);
        assert!(got.contains(want), "{sql}\n  want {want:?}\n  got  {got:?}");
    }
}

#[test]
fn untyped_literals_adopt_the_element_type() {
    let mut e = Engine::new();
    // Converts.
    assert_eq!(text(&mut e, "SELECT ARRAY[1,'2']"), "{1,2}");
    assert_eq!(text(&mut e, "SELECT pg_typeof(ARRAY[1,'2'])::text"), "integer[]");
    // Doesn't convert: PG reports the value, not a type mismatch.
    let got = err(&mut e, "SELECT ARRAY[1,'a']");
    assert!(
        got.contains("invalid input syntax for type integer: \"a\""),
        "{got}"
    );
    // NULL is untyped too and never blocks the resolution.
    assert_eq!(text(&mut e, "SELECT ARRAY[1,NULL]"), "{1,NULL}");
    assert_eq!(text(&mut e, "SELECT pg_typeof(ARRAY[1,NULL])::text"), "integer[]");
    // All-untyped stays text.
    assert_eq!(text(&mut e, "SELECT pg_typeof(ARRAY['a','b'])::text"), "text[]");
    assert_eq!(text(&mut e, "SELECT ARRAY[NULL,NULL]"), "{NULL,NULL}");
}

#[test]
fn elements_within_a_type_family_still_unify() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT ARRAY[1,2.5]"), "{1,2.5}");
    assert_eq!(text(&mut e, "SELECT pg_typeof(ARRAY[1,2.5])::text"), "numeric[]");
    assert_eq!(text(&mut e, "SELECT ARRAY[1::bigint,2::int]"), "{1,2}");
    assert_eq!(
        text(&mut e, "SELECT pg_typeof(ARRAY[1::bigint,2::int])::text"),
        "bigint[]"
    );
    assert_eq!(text(&mut e, "SELECT ARRAY[true,false]"), "{t,f}");
}

#[test]
fn the_everyday_array_surface_is_unchanged() {
    let mut e = Engine::new();
    // Regression guard over the sweep's clean cases — these matched PG
    // before this round and must still.
    for (sql, want) in [
        ("SELECT (ARRAY[1,2,3])[2]::text", "2"),
        ("SELECT array_to_string((ARRAY[1,2,3])[1:2],',')", "1,2"),
        ("SELECT array_to_string(array_append(ARRAY[1,2],3),',')", "1,2,3"),
        ("SELECT array_to_string(array_remove(ARRAY[1,2,2,3],2),',')", "1,3"),
        ("SELECT array_to_string(array_cat(ARRAY[1,2],ARRAY[3]),',')", "1,2,3"),
        ("SELECT array_length(ARRAY[1,2,3],1)::text", "3"),
        ("SELECT cardinality(ARRAY[1,2,3])::text", "3"),
        ("SELECT array_to_string(ARRAY[1,NULL,3],',','X')", "1,X,3"),
        ("SELECT array_to_string(string_to_array('a,b,,c',','),'|')", "a|b||c"),
        ("SELECT array_to_string(ARRAY[[1,2],[3,4]],',')", "1,2,3,4"),
    ] {
        assert_eq!(text(&mut e, sql), want, "{sql}");
    }
}
