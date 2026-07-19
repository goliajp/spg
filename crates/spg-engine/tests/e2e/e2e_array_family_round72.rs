//! v7.39 (read01 round 72) — the array-function family, and the fallback that
//! was hiding underneath it.
//!
//! Round 71 found `array_remove` missing its TEXT-array arm and pointed here.
//! Sweeping the family across element types found `array_replace` (int arms
//! only) and `array_to_string` (no NumericArray arm) — the same gap, in the
//! functions next door.
//!
//! But the sweep also found the thing UNDER them. `ARRAY[true, false]` was a
//! **text[]**: the array-literal's element typing had arms for the numeric
//! ladder and text, and everything else fell into
//!
//!     _ => has_text = true
//!
//! A silent degradation, not a decision. It usually LOOKED right, because
//! `array_to_string` renders `t` either way — which is exactly what let it sit
//! for so long. The array functions are what tripped over it: a `Bool` needle
//! could not match a `TextArray` element.
//!
//! The two fixes are the same fix, applied at two depths: stop writing these
//! per-variant. `array_element_at` + a new `array_rebuild` make the element type
//! somebody else's problem, and a homogeneous literal keeps its own type.
//!
//! Byte-locked against live PG18.4.

use spg_engine::{Engine, QueryResult};

fn r1(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn a_homogeneous_literal_keeps_its_type() {
    let mut e = Engine::new();
    assert_eq!(
        r1(&mut e, "SELECT pg_typeof(ARRAY[true,false])"),
        "boolean[]"
    );
    assert_eq!(
        r1(&mut e, "SELECT pg_typeof(ARRAY['2024-01-01'::date])"),
        "date[]"
    );
    // The numeric ladder and text are unchanged.
    assert_eq!(r1(&mut e, "SELECT pg_typeof(ARRAY[1,2])"), "integer[]");
    assert_eq!(r1(&mut e, "SELECT pg_typeof(ARRAY[1,2.5])"), "numeric[]");
    assert_eq!(r1(&mut e, "SELECT pg_typeof(ARRAY['a','b'])"), "text[]");
    // v7.39 (round 236) — a MIX used to fall back to text[]; PG resolves
    // the untyped `'x'` against the boolean elements and reports that it
    // will not convert. `'t'` does convert, and the array stays boolean[].
    let got = e
        .execute("SELECT pg_typeof(ARRAY[true,'x'])")
        .expect_err("an unconvertible untyped element must be rejected")
        .to_string();
    assert!(
        got.contains("invalid input syntax for type boolean: \"x\""),
        "{got}"
    );
    assert_eq!(r1(&mut e, "SELECT pg_typeof(ARRAY[true,'t'])"), "boolean[]");
}

#[test]
fn array_remove_works_on_every_element_type() {
    let mut e = Engine::new();
    assert_eq!(
        r1(
            &mut e,
            "SELECT array_to_string(array_remove(ARRAY['a','b','a'],'a'),'|')"
        ),
        "b"
    );
    assert_eq!(
        r1(
            &mut e,
            "SELECT array_to_string(array_remove(ARRAY[true,false,true],false),'|')"
        ),
        "t|t"
    );
    assert_eq!(
        r1(
            &mut e,
            "SELECT array_to_string(array_remove(ARRAY[1.5,2.5],1.5),'|')"
        ),
        "2.5"
    );
    assert_eq!(
        r1(
            &mut e,
            "SELECT array_to_string(array_remove(ARRAY['2024-01-01'::date,'2024-02-01'::date],'2024-01-01'::date),'|')"
        ),
        "2024-02-01"
    );
}

#[test]
fn array_replace_works_on_every_element_type() {
    let mut e = Engine::new();
    assert_eq!(
        r1(
            &mut e,
            "SELECT array_to_string(array_replace(ARRAY['a','b','a'],'a','z'),'|')"
        ),
        "z|b|z"
    );
    assert_eq!(
        r1(
            &mut e,
            "SELECT array_to_string(array_replace(ARRAY[true,false],false,true),'|')"
        ),
        "t|t"
    );
    assert_eq!(
        r1(
            &mut e,
            "SELECT array_to_string(array_replace(ARRAY[1,2,1],1,9),'|')"
        ),
        "9|2|9"
    );
}

#[test]
fn a_bool_element_renders_as_t_inside_an_array() {
    // The ARRAY rendering, not the scalar one — `t`, not `true`.
    let mut e = Engine::new();
    assert_eq!(
        r1(&mut e, "SELECT array_to_string(ARRAY[true,false],'|')"),
        "t|f"
    );
}

#[test]
fn array_to_string_handles_a_numeric_array() {
    let mut e = Engine::new();
    assert_eq!(
        r1(&mut e, "SELECT array_to_string(ARRAY[1.5,2.5],'|')"),
        "1.5|2.5"
    );
}
