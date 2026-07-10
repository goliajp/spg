//! v7.38 (read01) — expectations match PG's actual similar_to_escape output
//! (`^(?:...)$`, groups turned non-capturing). The prior `^...$` forms never
//! matched PG.
//! v7.37.17 (17.6 siblings) — similar_to_escape(pattern [, escape]).

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn similar_to_escape_basic_wildcards() {
    let mut e = Engine::new();
    assert_eq!(
        text(&first(&mut e, "SELECT similar_to_escape('abc%')")),
        "^(?:abc.*)$"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT similar_to_escape('a_c')")),
        "^(?:a.c)$"
    );
    assert_eq!(
        text(&first(&mut e, "SELECT similar_to_escape('a%b_c')")),
        "^(?:a.*b.c)$"
    );
}

#[test]
fn similar_to_escape_regex_meta_escaped() {
    let mut e = Engine::new();
    // Dot in SIMILAR TO is a literal, must escape for regex.
    assert_eq!(
        text(&first(&mut e, "SELECT similar_to_escape('a.b')")),
        "^(?:a\\.b)$"
    );
    // Character-class-like brackets pass through.
    assert_eq!(
        text(&first(&mut e, "SELECT similar_to_escape('[abc]')")),
        "^(?:[abc])$"
    );
    // Alternation passes through.
    assert_eq!(
        text(&first(&mut e, "SELECT similar_to_escape('(a|b)')")),
        "^(?:(?:a|b))$"
    );
}

#[test]
fn similar_to_escape_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT similar_to_escape(NULL::text)"),
        spg_storage::Value::Null
    ));
}
