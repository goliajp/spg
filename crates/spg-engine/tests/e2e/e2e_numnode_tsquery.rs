//! v7.38 (read01 P6.31) — numnode() counts the nodes of a real tsquery value
//! (lexemes + operators). Oracle behaviour from live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn int(e: &mut Engine, sql: &str) -> i32 {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Int(n) => *n,
            v => panic!("expected int, got {v:?}"),
        },
        _ => panic!("expected rows"),
    }
}

#[test]
fn numnode_counts_tsquery_nodes() {
    let mut e = Engine::new();
    // PG: numnode('a & b | c') = 5 (3 terms + AND + OR)
    assert_eq!(int(&mut e, "SELECT numnode('a & b | c'::tsquery)"), 5);
    assert_eq!(int(&mut e, "SELECT numnode('a'::tsquery)"), 1);
    // !a & b = AND + NOT + a + b
    assert_eq!(int(&mut e, "SELECT numnode('!a & b'::tsquery)"), 4);
    // Also works on a query built by plainto_tsquery.
    assert_eq!(
        int(&mut e, "SELECT numnode(plainto_tsquery('simple','a b c'))"),
        5
    );
}
