//! v7.37.17 (17.6 siblings) — PG 14+ string_to_table + PG 8.3+
//! regexp_split_to_table as FROM-position SRFs (row streams over
//! the existing *_to_array scalars).

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.into_iter()
        .map(|row| row.values.into_iter().collect())
        .collect()
}

fn texts(got: &[Vec<spg_storage::Value<'static>>]) -> Vec<Option<String>> {
    got.iter()
        .map(|r| match &r[0] {
            spg_storage::Value::Text(s) => Some(s.to_string()),
            spg_storage::Value::Null => None,
            other => panic!("expected Text/Null, got {other:?}"),
        })
        .collect()
}

#[test]
fn string_to_table_basic() {
    let mut e = Engine::new();
    // PG doc vector: string_to_table('xx~~yy~~zz', '~~') → xx / yy / zz.
    let got = rows(
        &mut e,
        "SELECT s FROM string_to_table('xx~~yy~~zz', '~~') AS s",
    );
    assert_eq!(
        texts(&got),
        [
            Some("xx".to_string()),
            Some("yy".to_string()),
            Some("zz".to_string())
        ]
    );
}

#[test]
fn string_to_table_null_string_form() {
    let mut e = Engine::new();
    // 3-arg form: matching elements become SQL NULL.
    let got = rows(
        &mut e,
        "SELECT v FROM string_to_table('a,none,c', ',', 'none') AS t(v)",
    );
    assert_eq!(
        texts(&got),
        [Some("a".to_string()), None, Some("c".to_string())]
    );
}

#[test]
fn regexp_split_to_table_basic() {
    let mut e = Engine::new();
    // PG doc vector shape: split on whitespace runs.
    let got = rows(
        &mut e,
        "SELECT w FROM regexp_split_to_table('the quick  brown', '\\s+') AS w",
    );
    assert_eq!(
        texts(&got),
        [
            Some("the".to_string()),
            Some("quick".to_string()),
            Some("brown".to_string())
        ]
    );
}

#[test]
fn count_composes() {
    let mut e = Engine::new();
    let got = rows(
        &mut e,
        "SELECT COUNT(*) FROM string_to_table('a b c d', ' ')",
    );
    assert!(matches!(
        got[0][0],
        spg_storage::Value::Int(4) | spg_storage::Value::BigInt(4)
    ));
}
