//! v7.38.19 — `@>` stopped building a document for the RIGHT side.
//!
//! v7.38.9 stopped building one for the LEFT, which is the big one.
//! What stayed was the constant on the right, rebuilt on every matched
//! row, plus one more parse per value located on the left.
//!
//! `@>` rides the GIN index, so the cost that shows is the recheck per
//! MATCHED row. On sentori's 200,000-row `events`:
//!
//!     traits @> '{"plan":"pro"}'                 66,667 rows   18.713 ms   PG 8.193
//!     traits @> '{"plan":"pro","country":"jp"}'  16,667 rows    8.154      PG 4.387
//!     … plus "version":"7"                            0 rows    0.631      PG 0.808
//!
//! The last line named it: with nothing to recheck we are FASTER than
//! PostgreSQL. The whole gap was per-matched-row, and it grew about
//! 70 ns for each key added to the constant.
//!
//! Every expectation below is PostgreSQL 18.4's, taken from it and not
//! reasoned about. Two of them are why the fast path is narrow:
//!
//!   * `num-scale` — `{"a":1.00} @> {"a":1.0}` is TRUE and the tokens
//!     differ, because jsonb numbers compare numerically while keeping
//!     the scale they were written with. Numbers go to the parser.
//!   * `esc-quote` — a string carrying an escape is refused from the
//!     other direction: two spellings, one string.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Bool(b) => b.to_string(),
            spg_storage::Value::Text(t) => t.to_string(),
            spg_storage::Value::Null => "<NULL>".into(),
            other => format!("{other:?}"),
        },
        other => panic!("expected rows from {sql}, got {other:?}"),
    }
}

#[test]
fn containment_matches_postgresql() {
    let mut e = Engine::new();
    for (case, lhs, rhs, want) in [
        ("flat-hit", r#"{"a":"x","b":"y"}"#, r#"{"a":"x"}"#, "true"),
        ("flat-miss-val", r#"{"a":"x"}"#, r#"{"a":"z"}"#, "false"),
        ("flat-miss-key", r#"{"a":"x"}"#, r#"{"c":"x"}"#, "false"),
        (
            "flat-both",
            r#"{"a":"x","b":"y"}"#,
            r#"{"a":"x","b":"y"}"#,
            "true",
        ),
        (
            "flat-partial",
            r#"{"a":"x","b":"y"}"#,
            r#"{"a":"x","b":"z"}"#,
            "false",
        ),
        ("empty-rhs", r#"{"a":"x"}"#, r#"{}"#, "true"),
        ("empty-both", r#"{}"#, r#"{}"#, "true"),
        ("empty-lhs", r#"{}"#, r#"{"a":"x"}"#, "false"),
        ("bool-true", r#"{"a":true}"#, r#"{"a":true}"#, "true"),
        ("bool-cross", r#"{"a":true}"#, r#"{"a":false}"#, "false"),
        ("null-val", r#"{"a":null}"#, r#"{"a":null}"#, "true"),
        ("null-vs-miss", r#"{"b":1}"#, r#"{"a":null}"#, "false"),
        ("str-vs-bool", r#"{"a":"true"}"#, r#"{"a":true}"#, "false"),
        ("str-vs-num", r#"{"a":"1"}"#, r#"{"a":1}"#, "false"),
        ("num-scale", r#"{"a":1.00}"#, r#"{"a":1.0}"#, "true"),
        ("num-int", r#"{"a":1.00}"#, r#"{"a":1}"#, "true"),
        ("num-exp", r#"{"a":1e2}"#, r#"{"a":100}"#, "true"),
        ("num-miss", r#"{"a":1}"#, r#"{"a":2}"#, "false"),
        ("esc-quote", r#"{"a":"q\"z"}"#, r#"{"a":"q\"z"}"#, "true"),
        ("esc-key", r#"{"a\"b":"x"}"#, r#"{"a\"b":"x"}"#, "true"),
        ("nested-rhs", r#"{"a":{"b":1}}"#, r#"{"a":{"b":1}}"#, "true"),
        (
            "nested-partial",
            r#"{"a":{"b":1,"c":2}}"#,
            r#"{"a":{"b":1}}"#,
            "true",
        ),
        ("arr-rhs", r#"{"a":[1,2,3]}"#, r#"{"a":[1,2]}"#, "true"),
        ("lhs-array", r#"[1,2,3]"#, r#"2"#, "true"),
        ("lhs-array-arr", r#"[1,[2,3]]"#, r#"[2,3]"#, "false"),
        ("scalar-both", r#""x""#, r#""x""#, "true"),
        ("ws-rhs", r#"{"a":"x"}"#, r#"{ "a" : "x" }"#, "true"),
        ("deep-key-only", r#"{"a":{"b":1}}"#, r#"{"b":1}"#, "false"),
    ] {
        let sql = format!("SELECT '{lhs}'::jsonb @> '{rhs}'");
        assert_eq!(one(&mut e, &sql), want, "{case}: {sql}");
    }
}

/// The right-hand side used to be parsed before anything else, so an
/// unparseable constant was an error. A bytes-only path that stops at
/// the closing brace would answer `true` for the first of these and
/// leave the operator quietly accepting what it used to reject.
#[test]
fn a_malformed_right_side_is_still_an_error() {
    let mut e = Engine::new();
    for rhs in [r#"{"a":"x"} garbage"#, r#"{"a":"x""#, r#"{"a":}"#] {
        let sql = format!(r#"SELECT '{{"a":"x"}}'::jsonb @> '{rhs}'::text"#);
        assert!(
            e.execute(&sql).is_err(),
            "must not answer a malformed right side: {sql}"
        );
    }
}

/// NULL on either side is NULL, not false — the fast path returns
/// before it can decide anything.
#[test]
fn null_stays_null() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, r#"SELECT NULL::jsonb @> '{"a":1}'"#), "<NULL>");
    assert_eq!(one(&mut e, r#"SELECT '{"a":1}'::jsonb @> NULL"#), "<NULL>");
}
