//! v7.39 (read01 commands/, round 50) — the COMMENT ON epic. COMMENT ON
//! was swallowed by the parser's dump-noise arm, so a comment was accepted
//! and thrown away and obj_description / col_description always returned
//! NULL. Comments now live in the catalog (FILE_VERSION 61), are readable
//! through obj_description / col_description / pg_description, and the
//! statement errors on a missing object like PG.

use spg_engine::{Engine, QueryResult};

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).unwrap_err())
}

fn ok(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn table_and_column_comments_round_trip() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE g1(a int, b text)");
    ok(&mut e, "COMMENT ON TABLE g1 IS 'my table'");
    assert_eq!(
        col(&mut e, "SELECT obj_description('g1'::regclass, 'pg_class')"),
        vec!["my table"]
    );
    ok(&mut e, "COMMENT ON COLUMN g1.a IS 'the a col'");
    assert_eq!(
        col(&mut e, "SELECT col_description('g1'::regclass, 1)"),
        vec!["the a col"]
    );
    // IS NULL removes it.
    ok(&mut e, "COMMENT ON TABLE g1 IS NULL");
    assert_eq!(
        col(
            &mut e,
            "SELECT obj_description('g1'::regclass, 'pg_class') IS NULL"
        ),
        vec!["true"]
    );
}

#[test]
fn comment_on_missing_object_errors() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE g1(a int)");
    // Used to succeed silently.
    assert!(
        err(&mut e, "COMMENT ON TABLE nope_tbl IS 'x'")
            .contains("relation \"nope_tbl\" does not exist")
    );
    assert!(
        err(&mut e, "COMMENT ON COLUMN g1.nope IS 'x'")
            .contains("column \"nope\" of relation \"g1\" does not exist")
    );
}

#[test]
fn pg_description_view() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE pd1(a int, b text)");
    ok(&mut e, "COMMENT ON TABLE pd1 IS 'tbl c'");
    ok(&mut e, "COMMENT ON COLUMN pd1.b IS 'col c'");
    // objsubid 0 = the relation itself; 2 = its second column.
    match e
        .execute(
            "SELECT objsubid, description FROM pg_description \
             WHERE objoid = 'pd1'::regclass ORDER BY objsubid",
        )
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => {
            let got: Vec<String> = rows
                .iter()
                .map(|r| {
                    format!(
                        "{}|{}",
                        spg_engine::eval::value_to_text(&r.values[0]),
                        spg_engine::eval::value_to_text(&r.values[1])
                    )
                })
                .collect();
            assert_eq!(got, vec!["0|tbl c", "2|col c"]);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn dropping_the_table_purges_its_comments() {
    let mut e = Engine::new();
    ok(&mut e, "CREATE TABLE g1(a int)");
    ok(&mut e, "COMMENT ON TABLE g1 IS 'old'");
    ok(&mut e, "COMMENT ON COLUMN g1.a IS 'old col'");
    ok(&mut e, "DROP TABLE g1");
    // A new table of the same name must not inherit the stale comments.
    ok(&mut e, "CREATE TABLE g1(a int)");
    assert_eq!(
        col(
            &mut e,
            "SELECT obj_description('g1'::regclass, 'pg_class') IS NULL"
        ),
        vec!["true"]
    );
    assert_eq!(
        col(&mut e, "SELECT col_description('g1'::regclass, 1) IS NULL"),
        vec!["true"]
    );
}
