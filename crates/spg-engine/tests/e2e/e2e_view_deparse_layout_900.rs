//! 9.0.0 — a view over a join came back as one line of stored text.
//!
//! `pg_get_viewdef` lays the definition out: each projection item on its
//! own line, the FROM as a parenthesised join tree, the predicate in the
//! catalog form. SPG laid out only the shapes with no join; anything
//! else went to the fallback, which is the STORED body — one line, with
//! `INNER JOIN`, `AS` aliases and an untyped constant.
//!
//! Worse, the stored body IS the AST's `Display`, and that rendering
//! dropped three spellings: `FROM a, b` came back as `CROSS JOIN`,
//! `JOIN … USING (id)` as an ON predicate, and a NATURAL join as a join
//! with NO condition — a different query, which a restore would run.
//!
//! Measured on PostgreSQL 18.6:
//!
//! ```text
//!    SELECT a.id,
//!        a.name,
//!        b.amt
//!       FROM (d1a a
//!         JOIN d1b b ON ((a.id = b.id)))
//!      WHERE (b.amt > (0)::numeric);
//!
//!       FROM ((d1a a            -- one pair per join in the tree
//!         JOIN d1b b ON ((a.id = b.id)))
//!         JOIN d1c c ON ((c.id = a.id)));
//!
//!       FROM d1a a,             -- a comma starts a new item
//!        (d1b b
//!         JOIN d1c c ON ((b.id = c.id)));
//! ```

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
        "CREATE TABLE d1a (id int, name text)",
        "CREATE TABLE d1b (id int, amt numeric)",
        "CREATE TABLE d1c (id int, z text)",
        "CREATE VIEW v1 AS SELECT a.id, a.name, b.amt FROM d1a a JOIN d1b b ON a.id=b.id WHERE b.amt > 0",
        "CREATE VIEW v2 AS SELECT id, name FROM d1a WHERE name LIKE 'x%' ORDER BY id",
        "CREATE VIEW v3 AS SELECT a.id FROM d1a a JOIN d1b b ON a.id=b.id JOIN d1c c ON c.id=a.id",
        "CREATE VIEW v4 AS SELECT a.id FROM d1a a, d1b b WHERE a.id=b.id",
        "CREATE VIEW v5 AS SELECT a.id FROM d1a a CROSS JOIN d1b b",
        "CREATE VIEW v6 AS SELECT a.id FROM d1a a LEFT JOIN d1b b ON a.id=b.id",
        "CREATE VIEW v7 AS SELECT a.id FROM d1a a JOIN d1b b USING (id)",
        "CREATE VIEW v8 AS SELECT a.id FROM d1a a NATURAL JOIN d1b b",
        "CREATE VIEW v9 AS SELECT a.id FROM d1a a, d1b b JOIN d1c c ON b.id=c.id",
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
fn a_join_is_laid_out_and_its_predicate_is_analysed() {
    let mut e = seeded();
    assert_eq!(
        viewdef(&mut e, "v1"),
        " SELECT a.id,\n    a.name,\n    b.amt\n   FROM (d1a a\n     JOIN d1b b ON ((a.id = b.id)))\n  WHERE (b.amt > (0)::numeric);"
    );
}

#[test]
fn a_predicate_with_no_join_is_analysed_too() {
    // This shape was laid out before and the predicate was not: PG reads
    // `~~` and a typed literal, SPG read `LIKE 'x%'`.
    let mut e = seeded();
    assert_eq!(
        viewdef(&mut e, "v2"),
        " SELECT id,\n    name\n   FROM d1a\n  WHERE (name ~~ 'x%'::text)\n  ORDER BY id;"
    );
}

#[test]
fn a_tree_of_joins_opens_one_pair_per_join() {
    let mut e = seeded();
    assert_eq!(
        viewdef(&mut e, "v3"),
        " SELECT a.id\n   FROM ((d1a a\n     JOIN d1b b ON ((a.id = b.id)))\n     JOIN d1c c ON ((c.id = a.id)));"
    );
}

#[test]
fn a_comma_is_not_a_cross_join() {
    let mut e = seeded();
    assert_eq!(
        viewdef(&mut e, "v4"),
        " SELECT a.id\n   FROM d1a a,\n    d1b b\n  WHERE (a.id = b.id);"
    );
    assert_eq!(
        viewdef(&mut e, "v5"),
        " SELECT a.id\n   FROM (d1a a\n     CROSS JOIN d1b b);"
    );
    // A comma starts a new item; the join after it belongs to that item.
    assert_eq!(
        viewdef(&mut e, "v9"),
        " SELECT a.id\n   FROM d1a a,\n    (d1b b\n     JOIN d1c c ON ((b.id = c.id)));"
    );
}

#[test]
fn using_and_natural_keep_their_condition() {
    let mut e = seeded();
    assert_eq!(
        viewdef(&mut e, "v7"),
        " SELECT a.id\n   FROM (d1a a\n     JOIN d1b b USING (id));"
    );
    // PostgreSQL deparses NATURAL as the USING it resolved to.
    assert_eq!(
        viewdef(&mut e, "v8"),
        " SELECT a.id\n   FROM (d1a a\n     JOIN d1b b USING (id));"
    );
}

#[test]
fn the_outer_join_keyword_is_the_one_postgresql_writes() {
    let mut e = seeded();
    assert_eq!(
        viewdef(&mut e, "v6"),
        " SELECT a.id\n   FROM (d1a a\n     LEFT JOIN d1b b ON ((a.id = b.id)));"
    );
}
