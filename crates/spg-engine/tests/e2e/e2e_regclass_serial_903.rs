//! 9.0.3 — a serial column's sequence printed as a regclass carried its
//! schema even where the bare name reaches it: `public.st_id_seq` where
//! PostgreSQL 18.6 prints `st_id_seq`. The rule for every relation is to
//! write the schema only when the bare name would reach something else;
//! the check looked in the catalog's registries, and a serial column's
//! own sequence is in none of them. Expected values are PostgreSQL's.

use spg_engine::{Engine, QueryResult};

fn texts(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    else {
        panic!("{sql}: expected rows");
    };
    rows.iter()
        .map(|r| match &r.values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            other => panic!("{sql}: not text: {other:?}"),
        })
        .collect()
}

#[test]
fn a_serial_sequence_is_written_bare_where_the_bare_name_reaches_it() {
    let mut e = Engine::new();
    for sql in [
        "CREATE SCHEMA sa",
        "CREATE TABLE st (id serial, v text)",
        "CREATE SEQUENCE s1",
        "CREATE TABLE sa.x (id serial)",
    ] {
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    }
    assert_eq!(
        texts(
            &mut e,
            "SELECT seqrelid::regclass::text FROM pg_sequence ORDER BY 1"
        ),
        ["s1", "sa.x_id_seq", "st_id_seq"]
    );
    assert_eq!(
        texts(&mut e, "SELECT 'st_id_seq'::regclass::text"),
        ["st_id_seq"]
    );
    e.execute("SET search_path = sa, public").unwrap();
    assert_eq!(
        texts(
            &mut e,
            "SELECT c.oid::regclass::text FROM pg_class c \
             WHERE relname IN ('st', 'st_id_seq', 'x', 'x_id_seq') ORDER BY 1"
        ),
        ["st", "st_id_seq", "x", "x_id_seq"]
    );
}
