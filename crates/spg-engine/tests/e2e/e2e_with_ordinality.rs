//! unnest(...) WITH ORDINALITY — trailing 1-based BIGINT row
//! counter, PG 9.4+.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.iter().map(|row| row.values.to_vec()).collect()
}

fn cols(e: &mut Engine, sql: &str) -> Vec<String> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { columns, .. } = r else {
        panic!("expected Rows");
    };
    columns.iter().map(|c| c.name.clone()).collect()
}

fn as_i64(v: &spg_storage::Value<'_>) -> i64 {
    match v {
        spg_storage::Value::Int(n) => i64::from(*n),
        spg_storage::Value::BigInt(n) => *n,
        other => panic!("expected integer, got {other:?}"),
    }
}

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected text, got {other:?}"),
    }
}

#[test]
fn ordinality_counts_from_one() {
    let mut e = Engine::new();
    let got = rows(
        &mut e,
        "SELECT * FROM unnest(string_to_array('a,b,c', ',')) WITH ORDINALITY",
    );
    assert_eq!(got.len(), 3);
    for (i, r) in got.iter().enumerate() {
        assert_eq!(as_i64(&r[1]), i as i64 + 1);
    }
    assert_eq!(text(&got[2][0]), "c");
    // PG's default column name.
    assert_eq!(
        cols(
            &mut e,
            "SELECT * FROM unnest(string_to_array('a', ',')) WITH ORDINALITY"
        )[1],
        "ordinality"
    );
}

#[test]
fn column_aliases_rename_both() {
    let mut e = Engine::new();
    // AS t(x, ord) — first renames the element, second the counter.
    let got = rows(
        &mut e,
        "SELECT ord, x FROM unnest(string_to_array('p,q', ',')) \
         WITH ORDINALITY AS t(x, ord) ORDER BY ord DESC",
    );
    assert_eq!(got.len(), 2);
    assert_eq!(as_i64(&got[0][0]), 2);
    assert_eq!(text(&got[0][1]), "q");
}

#[test]
fn srf_rewrite_arm_gets_ordinality() {
    let mut e = Engine::new();
    // jsonb_array_elements_text goes through the FROM-SRF rewrite
    // arm — WITH ORDINALITY must ride along.
    let got = rows(
        &mut e,
        "SELECT value, ordinality \
         FROM jsonb_array_elements_text('[\"x\",\"y\",\"z\"]') WITH ORDINALITY",
    );
    assert_eq!(got.len(), 3);
    assert_eq!(text(&got[1][0]), "y");
    assert_eq!(as_i64(&got[1][1]), 2);
}

#[test]
fn ordinality_in_join_position() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE j (k INT)").unwrap();
    e.execute("INSERT INTO j VALUES (1)").unwrap();
    // The unnest sits in a FROM list next to a real table — the
    // materialise path must append the counter too.
    let got = rows(
        &mut e,
        "SELECT t.ord FROM j, unnest(string_to_array('a,b', ',')) \
         WITH ORDINALITY AS t(v, ord) ORDER BY t.ord",
    );
    assert_eq!(got.len(), 2);
    assert_eq!(as_i64(&got[0][0]), 1);
    assert_eq!(as_i64(&got[1][0]), 2);
}
