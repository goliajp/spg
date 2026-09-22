//! 9.0.3 — the embedded dump import read a COPY's head by hand: it
//! stripped the schema (`COPY sa.t (…) FROM stdin`, pg_dump's spelling
//! for every table, loaded `sa`'s rows into `public.t`), ignored every
//! option, skipped an empty line PostgreSQL reads as a row, and split the
//! statement at the first `;` even inside a quoted option.

use spg_embedded::Database;
use spg_storage::Value;

fn texts(db: &mut Database, sql: &str) -> Vec<Option<String>> {
    db.query(sql)
        .unwrap()
        .into_iter()
        .map(|r| match &r[0] {
            Value::Null => None,
            Value::Text(s) => Some(s.to_string()),
            other => panic!("{sql}: not text: {other:?}"),
        })
        .collect()
}

#[test]
fn a_dumps_copy_into_another_schema_fills_that_schemas_table() {
    let mut db = Database::open_in_memory();
    db.execute_script(
        "CREATE SCHEMA sa;
CREATE TABLE public.t (v text);
CREATE TABLE sa.t (v text);
COPY sa.t (v) FROM stdin;
in-sa
\\.
COPY public.t (v) FROM stdin;
in-public
\\.
",
    )
    .expect("the dump loads");
    assert_eq!(
        texts(&mut db, "SELECT v FROM sa.t"),
        [Some("in-sa".to_string())]
    );
    assert_eq!(
        texts(&mut db, "SELECT v FROM public.t"),
        [Some("in-public".to_string())]
    );
}

#[test]
fn a_dumps_copy_reads_its_options_and_its_empty_lines() {
    let mut db = Database::open_in_memory();
    db.execute_script(
        "CREATE TABLE p (a int, b text);
COPY p FROM stdin (DELIMITER ';');
1;x
\\.
CREATE TABLE one (v text);
COPY one (v) FROM stdin;
x

y
\\.
",
    )
    .expect("the script loads");
    assert_eq!(
        texts(&mut db, "SELECT b FROM p WHERE a = 1"),
        [Some("x".to_string())]
    );
    assert_eq!(
        texts(&mut db, "SELECT v FROM one ORDER BY v"),
        [
            Some(String::new()),
            Some("x".to_string()),
            Some("y".to_string())
        ]
    );
}
