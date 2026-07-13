//! v7.39 (read01 utils/adt, round 16) — jsonpath.c / like.c /
//! like_match.c / mac.c / mac8.c. Byte-locked vs PG18: the ::jsonpath
//! canonical form, LIKE trailing-escape error, like_escape(), macaddr8's
//! 6-byte input form, and the macaddr8→macaddr EUI-64 cast.

use spg_engine::{Engine, QueryResult};

fn row_of(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows[0]
            .values
            .iter()
            .map(spg_engine::eval::value_to_text)
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn jsonpath_canonical_form() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT '$.a[*] ? (@ > 2)'::jsonpath, '$.\"odd key\"'::jsonpath, \
             'strict $.a'::jsonpath, 'lax $[0 to last]'::jsonpath"
        ),
        vec![
            "$.\"a\"[*]?(@ > 2)",
            "$.\"odd key\"",
            "strict $.\"a\"",
            "$[0 to last]"
        ]
    );
    assert_eq!(
        row_of(
            &mut e,
            "SELECT '$ ? (@ like_regex \"^a\" flag \"i\")'::jsonpath"
        ),
        vec!["$?(@ like_regex \"^a\" flag \"i\")"]
    );
    let err = e.execute("SELECT '{\"a\":1}'::jsonpath").unwrap_err();
    assert!(
        format!("{err}").contains("syntax error at or near \"{\" of jsonpath input"),
        "{err}"
    );
}

#[test]
fn like_trailing_escape_and_like_escape_fn() {
    let mut e = Engine::new();
    let err = e.execute("SELECT 'ab' LIKE 'a\\'").unwrap_err();
    assert!(
        format!("{err}").contains("LIKE pattern must not end with escape character"),
        "{err}"
    );
    assert_eq!(
        row_of(
            &mut e,
            "SELECT like_escape('50%', '#'), like_escape('a_b', '\\'), \
             'abc' LIKE 'abc' ESCAPE ''"
        ),
        vec!["50%", "a_b", "true"]
    );
}

#[test]
fn macaddr8_six_byte_form_and_eui64_cast() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT macaddr8 '08:00:2b:01:02:03', \
             macaddr8_set7bit(macaddr8 '00:11:22:33:44:55'), \
             (macaddr8 '08:00:2b:ff:fe:01:02:03')::macaddr"
        ),
        vec![
            "08:00:2b:ff:fe:01:02:03",
            "02:11:22:ff:fe:33:44:55",
            "08:00:2b:01:02:03"
        ]
    );
    let err = e
        .execute("SELECT (macaddr8 '08:00:2b:01:02:03:04:05')::macaddr")
        .unwrap_err();
    assert!(
        format!("{err}").contains("macaddr8 data out of range to convert to macaddr"),
        "{err}"
    );
    let err = e.execute("SELECT macaddr '08:00:2b:01:02:zz'").unwrap_err();
    assert!(
        format!("{err}").contains("invalid input syntax for type macaddr: "),
        "{err}"
    );
}
