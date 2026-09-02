//! v7.39.11 — `||` with a `"char"` operand is refused, as PostgreSQL
//! refuses it.
//!
//! Reported by sentori against the published 7.39.10 image, where it
//! cost them a comparison query: the PostgreSQL side raised and ours
//! answered, so the diff was between an error and a result. Measured on
//! PostgreSQL 18.6 — the operator is genuinely ambiguous there:
//!
//! ```text
//!   SELECT 'x' || 'r'::"char"          ERROR: operator is not unique: unknown || "char"
//!   SELECT 'r'::"char" || 'x'          ERROR: operator is not unique: "char" || unknown
//!   SELECT 'x'::text || 'r'::"char"    ERROR: operator is not unique: text || "char"
//!   SELECT 'x' || 'r'::char(1)         xr
//!   SELECT ('r'::"char")::text || 'x'  rx
//! ```
//!
//! An answer more permissive than PostgreSQL's is one a caller cannot
//! act on: a script written against SPG then fails on the thing SPG
//! claims to be. The reporter found this once and did not sweep for the
//! class, and neither have we — it is one operator, pinned.

use spg_engine::{Engine, QueryResult};

fn scalar(e: &mut Engine, sql: &str) -> String {
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"))
    else {
        panic!("{sql}: expected Rows");
    };
    match &rows[0].values[0] {
        spg_storage::Value::Text(t) => t.to_string(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn refused(e: &mut Engine, sql: &str) -> String {
    let err = e
        .execute(sql)
        .err()
        .unwrap_or_else(|| panic!("{sql}: answered where PostgreSQL refuses"));
    format!("{err}")
}

#[test]
fn an_untyped_literal_on_the_left_is_ambiguous() {
    let mut e = Engine::new();
    let text = refused(&mut e, "SELECT 'x' || 'r'::\"char\"");
    assert!(
        text.contains("operator is not unique"),
        "expected PostgreSQL's own sentence, got {text}"
    );
}

#[test]
fn an_untyped_literal_on_the_right_is_ambiguous() {
    let mut e = Engine::new();
    assert!(refused(&mut e, "SELECT 'r'::\"char\" || 'x'").contains("operator is not unique"));
}

#[test]
fn text_beside_char_is_ambiguous_and_names_both_types() {
    let mut e = Engine::new();
    let text = refused(&mut e, "SELECT 'x'::text || 'r'::\"char\"");
    assert!(
        text.contains("text || \"char\""),
        "PostgreSQL names both operand types: {text}"
    );
}

#[test]
fn char_n_is_a_different_type_and_still_concatenates() {
    // `CHAR(n)` / `BPCHAR` is not `"char"`, and PostgreSQL answers `xr`.
    let mut e = Engine::new();
    assert_eq!(scalar(&mut e, "SELECT 'x' || 'r'::char(1)"), "xr");
}

#[test]
fn an_explicit_cast_resolves_it() {
    // The escape hatch PostgreSQL gives, and the one a caller writes.
    let mut e = Engine::new();
    assert_eq!(scalar(&mut e, "SELECT ('r'::\"char\")::text || 'x'"), "rx");
}

#[test]
fn ordinary_concatenation_is_untouched() {
    let mut e = Engine::new();
    assert_eq!(scalar(&mut e, "SELECT 'x' || 'r'::text"), "xr");
    assert_eq!(scalar(&mut e, "SELECT 'x' || 1"), "x1");
}
