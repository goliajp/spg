//! 9.0.0 (D9) — a view's `*` is expanded when the view is CREATED.
//!
//! PostgreSQL analyses a view definition at CREATE time and stores the
//! columns the relation had at that moment. SPG stored the text
//! `SELECT * FROM t` and resolved it on every read, so:
//!
//!   * adding a column to `t` changed what `v` returns — the same
//!     statement answering two ways;
//!   * `pg_get_viewdef` read back `SELECT * FROM t` where PostgreSQL
//!     reads the columns;
//!   * and there was no column-level dependency for `DROP COLUMN` to
//!     check, which is D8.
//!
//! Measured on PostgreSQL 18.6:
//!
//! ```text
//!   CREATE VIEW w1 AS SELECT * FROM wt1;          SELECT id, nm FROM wt1;
//!   CREATE VIEW w4 AS SELECT * FROM wt1 x;        SELECT id, nm FROM wt1 x;
//!   … SELECT * FROM wt1 a JOIN wt2 b ON …         SELECT a.id, a.nm, b.bid, b.amt
//!   … SELECT a.* FROM wt1 a JOIN wt2 b ON …       SELECT a.id, a.nm
//! ```
//!
//! One relation expands UNQUALIFIED even when it is aliased; a join
//! qualifies by the alias. Both read off 18.6.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    let QueryResult::Rows { rows, .. } = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    else {
        panic!("expected rows for {sql}");
    };
    spg_engine::eval::value_to_text(
        rows.first()
            .expect("one row")
            .values
            .first()
            .expect("one column"),
    )
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    for sql in [
        "CREATE TABLE wt1 (id int, nm text)",
        "CREATE TABLE wt2 (bid int, amt numeric)",
        "CREATE VIEW w1 AS SELECT * FROM wt1",
        "CREATE VIEW w2 AS SELECT * FROM wt1 a JOIN wt2 b ON a.id = b.bid",
        "CREATE VIEW w3 AS SELECT a.* FROM wt1 a JOIN wt2 b ON a.id = b.bid",
        "CREATE VIEW w4 AS SELECT * FROM wt1 x",
    ] {
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    }
    e
}

fn viewdef(e: &mut Engine, name: &str) -> String {
    one(e, &format!("SELECT pg_get_viewdef('{name}'::regclass)"))
}

#[test]
fn one_relation_expands_unqualified() {
    let mut e = seeded();
    assert_eq!(viewdef(&mut e, "w1"), " SELECT id,\n    nm\n   FROM wt1;");
    assert_eq!(viewdef(&mut e, "w4"), " SELECT id,\n    nm\n   FROM wt1 x;");
}

#[test]
fn a_join_expands_qualified_by_the_alias() {
    let mut e = seeded();
    assert_eq!(
        viewdef(&mut e, "w2"),
        " SELECT a.id,\n    a.nm,\n    b.bid,\n    b.amt\n   FROM (wt1 a\n     JOIN wt2 b ON ((a.id = b.bid)));"
    );
    assert_eq!(
        viewdef(&mut e, "w3"),
        " SELECT a.id,\n    a.nm\n   FROM (wt1 a\n     JOIN wt2 b ON ((a.id = b.bid)));"
    );
}

#[test]
fn a_column_added_later_does_not_join_the_view() {
    // The behaviour change, and the reason the expansion is worth it:
    // on PostgreSQL a view's shape is fixed at CREATE.
    let mut e = seeded();
    e.execute("ALTER TABLE wt1 ADD COLUMN extra int")
        .expect("add column");
    let QueryResult::Rows { columns, .. } = e.execute("SELECT * FROM w1").expect("select") else {
        panic!("expected rows")
    };
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["id", "nm"], "the view picked up a new column");
}

#[test]
fn the_view_still_answers_what_it_did() {
    let mut e = seeded();
    e.execute("INSERT INTO wt1 VALUES (1, 'a')")
        .expect("insert");
    let QueryResult::Rows { rows, .. } = e.execute("SELECT * FROM w1").expect("select") else {
        panic!("expected rows")
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]
            .values
            .iter()
            .map(spg_engine::eval::value_to_text)
            .collect::<Vec<_>>(),
        vec!["1".to_string(), "a".to_string()]
    );
}

#[test]
fn a_materialized_view_expands_too() {
    // Measured on PG 18.6: `CREATE MATERIALIZED VIEW mv AS SELECT *
    // FROM mvt` reads back `SELECT a, b FROM mvt`.
    let mut e = Engine::new();
    e.execute("CREATE TABLE mvt (a int, b int)").expect("t");
    e.execute("CREATE MATERIALIZED VIEW mv AS SELECT * FROM mvt")
        .expect("matview");
    assert_eq!(viewdef(&mut e, "mv"), " SELECT a,\n    b\n   FROM mvt;");
}

#[test]
fn a_body_whose_columns_are_not_the_catalogs_keeps_its_star() {
    // A subquery's or a table function's columns are not the catalog's
    // to read, and a WRONG expansion is worse than a late-bound one.
    let mut e = Engine::new();
    e.execute("CREATE TABLE st (a int)").expect("table");
    e.execute("CREATE VIEW sv AS SELECT * FROM generate_series(1, 3) g")
        .expect("view over a set-returning function");
    assert!(
        viewdef(&mut e, "sv").contains('*'),
        "{}",
        viewdef(&mut e, "sv")
    );
}
