//! 9.0.0 — a rename did not follow into the views that read the
//! renamed relation.
//!
//! `CREATE VIEW v AS SELECT * FROM t; ALTER TABLE t RENAME TO t2` left
//! `v` answering `relation "t" does not exist` and `pg_get_viewdef(v)`
//! still naming `t`. Measured against PostgreSQL 18.6, whose `v` keeps
//! working and whose definition reads `t2`. A view body is stored as
//! TEXT here and as a parse tree resolved to oids there.
//!
//! Not new in 9.0.0 — `ALTER TABLE … RENAME TO` has had it since views
//! existed; found while adding the VIEW / MATERIALIZED VIEW / TYPE
//! rename forms.
//!
//! A CTE of the same name SHADOWS the relation and must NOT be
//! rewritten, which is what stops this reusing `collect_read_tables`:
//! that walker over-collects on purpose, and over-collecting here would
//! change what the view means.

use spg_engine::{Engine, QueryResult};

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    else {
        panic!("expected rows for {sql}");
    };
    rows.into_iter()
        .map(|r| spg_engine::eval::value_to_text(r.values.first().expect("one column")))
        .collect()
}

#[test]
fn a_view_keeps_working_across_a_table_rename() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ft (id int, v text)").unwrap();
    e.execute("INSERT INTO ft VALUES (1,'a')").unwrap();
    e.execute("CREATE VIEW fv AS SELECT * FROM ft").unwrap();
    e.execute("ALTER TABLE ft RENAME TO ft2").unwrap();
    assert_eq!(
        col(&mut e, "SELECT count(*) FROM fv"),
        vec!["1".to_string()]
    );
    assert!(
        col(&mut e, "SELECT pg_get_viewdef('fv'::regclass)")[0].contains("ft2"),
        "the definition still names the old relation"
    );
}

#[test]
fn the_rewrite_reaches_a_subquery_and_a_join() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ft (id int)").unwrap();
    e.execute("CREATE TABLE fu (id int)").unwrap();
    e.execute("INSERT INTO ft VALUES (1)").unwrap();
    e.execute("INSERT INTO fu VALUES (1)").unwrap();
    e.execute(
        "CREATE VIEW fv AS SELECT count(*) AS n FROM ft JOIN fu ON fu.id = ft.id \
         WHERE ft.id IN (SELECT id FROM ft)",
    )
    .unwrap();
    e.execute("ALTER TABLE ft RENAME TO ft2").unwrap();
    assert_eq!(col(&mut e, "SELECT n FROM fv"), vec!["1".to_string()]);
}

#[test]
fn a_cte_of_the_same_name_shadows_the_relation_and_is_left_alone() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ft (id int)").unwrap();
    e.execute("INSERT INTO ft VALUES (1)").unwrap();
    // The CTE named `ft` is what this view reads; the table of that name
    // is not referenced at all, so the rename must not touch it.
    e.execute("CREATE VIEW fshadow AS WITH ft AS (SELECT 9 AS id) SELECT id FROM ft")
        .unwrap();
    e.execute("ALTER TABLE ft RENAME TO ft2").unwrap();
    assert_eq!(col(&mut e, "SELECT id FROM fshadow"), vec!["9".to_string()]);
    assert!(
        !col(&mut e, "SELECT pg_get_viewdef('fshadow'::regclass)")[0].contains("ft2"),
        "the CTE reference was rewritten"
    );
}

#[test]
fn a_view_over_a_view_follows_the_inner_views_rename() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ft (id int)").unwrap();
    e.execute("INSERT INTO ft VALUES (1)").unwrap();
    e.execute("CREATE VIEW inner_v AS SELECT * FROM ft")
        .unwrap();
    e.execute("CREATE VIEW outer_v AS SELECT count(*) AS n FROM inner_v")
        .unwrap();
    e.execute("ALTER VIEW inner_v RENAME TO inner_v2").unwrap();
    assert_eq!(col(&mut e, "SELECT n FROM outer_v"), vec!["1".to_string()]);
}

#[test]
fn a_from_item_aliased_to_the_old_name_is_left_alone() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ft (id int)").unwrap();
    e.execute("INSERT INTO ft VALUES (1)").unwrap();
    e.execute("CREATE TABLE fo (id int)").unwrap();
    e.execute("INSERT INTO fo VALUES (7)").unwrap();
    // `ft` here is an ALIAS for `fo`, so the view never reads the table
    // called `ft` and the rename must touch neither the FROM item nor
    // the qualifier. Measured: PostgreSQL 18.6 answers 7 and keeps
    // `FROM fo ft`.
    e.execute("CREATE VIEW fa AS SELECT ft.id AS x FROM fo AS ft")
        .unwrap();
    e.execute("ALTER TABLE ft RENAME TO ft2").unwrap();
    assert_eq!(col(&mut e, "SELECT x FROM fa"), vec!["7".to_string()]);
    let def = col(&mut e, "SELECT pg_get_viewdef('fa'::regclass)").remove(0);
    assert!(!def.contains("ft2"), "the alias was rewritten: {def}");
}
