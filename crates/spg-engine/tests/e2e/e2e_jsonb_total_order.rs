//! v7.38 (read01 P6.24) — ORDER BY / DISTINCT on jsonb use PG's type-aware
//! total order (Null < String < Number < Boolean < Array < Object, recursive),
//! not the text spelling of the value. Oracle order from live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn ordered(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[0] {
                spg_storage::Value::Json(s) => s.to_string(),
                v => format!("{v:?}"),
            })
            .collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn jsonb_order_by_type_rank() {
    let mut e = Engine::new();
    let got = ordered(
        &mut e,
        r#"SELECT x FROM (VALUES ('[1,2]'::jsonb),('{"a":1}'::jsonb),('null'::jsonb),('true'::jsonb),('5'::jsonb)) v(x) ORDER BY x"#,
    );
    assert_eq!(got, vec!["null", "5", "true", "[1, 2]", "{\"a\": 1}"]);
}

#[test]
fn jsonb_array_by_length_then_elements() {
    let mut e = Engine::new();
    assert_eq!(
        ordered(&mut e, "SELECT x FROM (VALUES ('[9]'::jsonb),('[1,2,3]'::jsonb)) v(x) ORDER BY x"),
        vec!["[9]", "[1, 2, 3]"]
    );
    assert_eq!(
        ordered(&mut e, "SELECT x FROM (VALUES ('[1,9]'::jsonb),('[1,2]'::jsonb)) v(x) ORDER BY x"),
        vec!["[1, 2]", "[1, 9]"]
    );
}

#[test]
fn jsonb_object_by_pair_count() {
    let mut e = Engine::new();
    assert_eq!(
        ordered(
            &mut e,
            r#"SELECT x FROM (VALUES ('{"a":1,"b":2}'::jsonb),('{"z":1}'::jsonb)) v(x) ORDER BY x"#
        ),
        vec!["{\"z\": 1}", "{\"a\": 1, \"b\": 2}"]
    );
}

#[test]
fn jsonb_distinct_unaffected() {
    let mut e = Engine::new();
    match e
        .execute("SELECT count(DISTINCT x) FROM (VALUES ('1'::jsonb),('1'::jsonb),('2'::jsonb)) v(x)")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0].values[0], spg_storage::Value::BigInt(2));
        }
        _ => panic!(),
    }
}
