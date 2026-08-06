//! Round 780 (F31-D1) — hstore is reachable by name. The type, its
//! storage variant, codec and both text conversions have existed
//! since v7.17.0, but the type-NAME map never listed it, so every
//! spelling ('a=>1'::hstore, a column declared hstore) answered
//! 'type "hstore" does not exist' and CREATE EXTENSION hstore warned
//! that nothing it supplies would work. All six probe shapes are
//! PG18-measured byte-identical now, including PG's first-wins rule
//! for duplicate keys.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => {
            spg_engine::eval::value_to_text(&rows[0].values[0])
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn round780_hstore_resolves_by_name() {
    let mut e = Engine::new();
    e.execute("CREATE EXTENSION IF NOT EXISTS hstore").unwrap();
    assert_eq!(one(&mut e, "SELECT 'a=>1, b=>2'::hstore"), "\"a\"=>\"1\", \"b\"=>\"2\"");
    // PG keeps the FIRST occurrence of a duplicate key.
    assert_eq!(one(&mut e, "SELECT 'a=>1, a=>2'::hstore"), "\"a\"=>\"1\"");
    assert_eq!(one(&mut e, "SELECT ''::hstore"), "");
    assert_eq!(one(&mut e, "SELECT 'a=>NULL'::hstore"), "\"a\"=>NULL");
    // A declared column round-trips and reports its type.
    e.execute("CREATE TABLE d1t (h hstore)").unwrap();
    e.execute("INSERT INTO d1t VALUES ('x=>1, y=>2')").unwrap();
    assert_eq!(one(&mut e, "SELECT h FROM d1t"), "\"x\"=>\"1\", \"y\"=>\"2\"");
    assert_eq!(one(&mut e, "SELECT pg_typeof(h) FROM d1t"), "hstore");
}
