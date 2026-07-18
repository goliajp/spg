//! v7.39 (round 223) — ARE regex edge differential vs live PG18.4
//! (2026-07-19). Back-references, lookahead, word boundaries and
//! embedded options all probed SAME (pinned here — none had pins);
//! the one gap fixed: a PCRE-style named group `(?<n>…)` / `(?P<n>…)`
//! or atomic group `(?>…)` used to fall through as a LITERAL `?` inside
//! a plain capturing group and silently match nothing. PG rejects them
//! (ARE has no such syntax); SPG now raises PG's two messages.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => format!("{:?}", rows[0].values[0]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn backreference_and_lookahead_match_pg() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT 'abcabc' ~ '(abc)\\1'"), "Bool(true)");
    assert_eq!(one(&mut e, "SELECT 'abcdef' ~ '(abc)\\1'"), "Bool(false)");
    assert_eq!(
        one(&mut e, "SELECT regexp_replace('foofoo bar', '(foo)\\1', 'X')"),
        "Text(\"X bar\")"
    );
    assert_eq!(one(&mut e, "SELECT 'foobar' ~ 'foo(?=bar)'"), "Bool(true)");
    assert_eq!(one(&mut e, "SELECT 'foobaz' ~ 'foo(?=bar)'"), "Bool(false)");
    assert_eq!(one(&mut e, "SELECT 'foobar' ~ 'foo(?!qux)'"), "Bool(true)");
    // Word boundaries (PG's \m \M).
    assert_eq!(
        one(&mut e, "SELECT 'word boundary' ~ '\\mword\\M'"),
        "Bool(true)"
    );
}

#[test]
fn embedded_options_match_pg() {
    let mut e = Engine::new();
    // (?i) case-insensitive and (?c) case-sensitive are ARE embedded options.
    assert_eq!(one(&mut e, "SELECT 'ABC' ~ '(?i)abc'"), "Bool(true)");
    assert_eq!(one(&mut e, "SELECT 'abc' ~ '(?c)abc'"), "Bool(true)");
    // (?x) expanded: whitespace ignored, so 'a b' does not contain 'ab'.
    assert_eq!(one(&mut e, "SELECT 'a b' ~ '(?x)a b'"), "Bool(false)");
}

#[test]
fn pcre_only_groups_rejected_like_pg() {
    let mut e = Engine::new();
    // Named group: PG reads the letter run as a (bad) embedded option.
    let err = e
        .execute("SELECT 'abc' ~ '(?P<n>a)'")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("invalid regular expression: invalid embedded option"),
        "{err}"
    );
    // (?<n>…) and atomic (?>…): a `?` with no quantifier operand.
    let err = e
        .execute("SELECT regexp_replace('hello', '(?<first>h)', 'H')")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("invalid regular expression: quantifier operand invalid"),
        "{err}"
    );
    let err = e.execute("SELECT 'abc' ~ '(?>a)'").unwrap_err().to_string();
    assert!(
        err.contains("invalid regular expression: quantifier operand invalid"),
        "{err}"
    );
}
