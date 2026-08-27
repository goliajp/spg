//! v7.39.2 — `COLLATE` on an expression is carried, not absorbed.
//!
//! The parser refused the locale names in this position and SILENTLY
//! DROPPED the byte-order ones. On an image whose database collates by
//! locale — the shipped default since v7.38.22 — that meant the one
//! family it let through was the one where dropping it changes the
//! answer. Measured against PostgreSQL 18.6, before:
//!
//! ```text
//!   SELECT 'a' COLLATE "C" < 'B'              PG f        SPG t
//!   SELECT 1 WHERE 'a' COLLATE "C" < 'B'      PG no rows  SPG 1
//!   GROUP BY x COLLATE "C" over ('a'),('A')   PG 2 groups SPG 1
//!   SELECT 'a' COLLATE "en_US.utf8" < 'B'     PG t        SPG error
//! ```
//!
//! `Expr::Collate` carries it and `collate_derive` — which already
//! modelled `Explicit(name)` and had no way to be handed one — reads it.
//! Every expectation below is PG 18.6's, measured.

use spg_engine::{Engine, QueryResult};

/// An engine whose DATABASE collates by locale — which is what the
/// shipped image does since v7.38.22 (`LANG=en_US.utf8`).
///
/// This is not decoration. `Engine::new()` collates by BYTES, and under
/// that database "absorb the clause" and "perform `COLLATE \"C\"`" give
/// the same answer — so a pin built on it passes whether or not the
/// clause is carried. The first draft of this file was built on it, and
/// an ablation that restored the absorption left every assertion green.
fn locale_db() -> Engine {
    let mut e = Engine::new();
    e.set_database_collation("en_US.utf8")
        .expect("the shipped image's own default");
    e
}

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(QueryResult::Rows { rows, .. }) if !rows.is_empty() => {
            spg_engine::eval::value_to_text(&rows[0].values[0])
        }
        Ok(_) => "<none>".to_string(),
        Err(err) => panic!("{sql}: {err}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).expect_err(sql))
}

#[test]
fn byte_order_and_a_locale_are_different_comparisons() {
    let mut e = locale_db();
    // The whole defect in two rows: absorbing the clause made these the
    // same answer, and PG says they differ.
    assert_eq!(one(&mut e, r#"SELECT 'a' COLLATE "C" < 'B'"#), "false");
    assert_eq!(
        one(&mut e, r#"SELECT 'a' COLLATE "en_US.utf8" < 'B'"#),
        "true"
    );
}

#[test]
fn it_reaches_where_group_by_and_the_select_list() {
    let mut e = locale_db();
    // WHERE — PG returns no rows for this.
    assert_eq!(
        one(&mut e, r#"SELECT 1 WHERE 'a' COLLATE "C" < 'B'"#),
        "<none>"
    );
    // The select list.
    assert_eq!(
        one(&mut e, r#"SELECT ('a' COLLATE "en_US.utf8") = 'A'"#),
        "false"
    );
    // GROUP BY — two groups under C, because 'a' and 'A' are two values
    // there. Absorbing the clause folded them into one.
    e.execute("CREATE TABLE g (x text)").unwrap();
    e.execute("INSERT INTO g VALUES ('a'), ('A')").unwrap();
    assert_eq!(
        one(
            &mut e,
            r#"SELECT count(*) FROM (SELECT count(*) AS c FROM g GROUP BY x COLLATE "C") s"#
        ),
        "2"
    );
}

#[test]
fn a_name_that_is_not_a_collation_is_refused_the_way_pg_refuses_it() {
    let mut e = Engine::new();
    assert!(
        err(&mut e, r#"SELECT 'a' COLLATE "nosuch_coll""#)
            .contains(r#"collation "nosuch_coll" for encoding "UTF8" does not exist"#),
        "PG 18.6's own sentence"
    );
}

#[test]
fn a_mysql_spelling_does_not_exist_on_the_postgresql_wire() {
    // `is_known` is deliberately a superset — SPG HAS the MySQL
    // collations and a column declared over one wire must be declarable
    // over the other. That argument is about declarations and does not
    // reach an expression inside one session, where PG's answer is that
    // there is no such collation.
    let mut e = locale_db();
    assert!(err(&mut e, "SELECT 'a' = 'A' COLLATE utf8mb4_bin").contains("does not exist"));
    // And the same session still performs the ones PG has.
    assert_eq!(one(&mut e, r#"SELECT 'a' = 'A' COLLATE "C""#), "false");
}

#[test]
fn a_schema_qualified_name_works_and_a_missing_schema_does_not() {
    // `pg_dump` writes `COLLATE pg_catalog.default`, and PG accepts a
    // qualified name whose schema exists — measured, `pg_catalog."en_US"`
    // answers `t` there.
    let mut e = locale_db();
    assert_eq!(
        one(&mut e, r#"SELECT 'a' = 'a' COLLATE pg_catalog."en_US""#),
        "true"
    );
    assert_eq!(
        one(&mut e, r#"SELECT 'a' < 'B' COLLATE pg_catalog."C""#),
        "false"
    );
    // The qualifier is dropped — SPG is single-schema — but it is READ
    // first, which is the difference between dropping it and ignoring
    // it. PG: `schema "nosuch_schema" does not exist`.
    assert!(
        err(&mut e, r#"SELECT 'a' = 'a' COLLATE nosuch_schema."C""#)
            .contains(r#"schema "nosuch_schema" does not exist"#)
    );
}

#[test]
fn the_index_key_still_refuses_what_it_cannot_carry() {
    // The negative control, and it is the one that matters: an index
    // key's collation rides a side channel that holds the byte-order
    // spellings only. Suppressing the node there without keeping this
    // check made `CREATE INDEX … (name COLLATE "en_US")` succeed — a
    // refusal that was doing real work, removed by a fix to something
    // else.
    let mut e = Engine::new();
    e.execute("CREATE TABLE ix (name text)").unwrap();
    assert!(
        err(&mut e, r#"CREATE INDEX bad ON ix (name COLLATE "en_US")"#)
            .contains("not supported in this position")
    );
    e.execute(r#"CREATE INDEX ok ON ix (name COLLATE "C")"#)
        .expect("the byte-order spelling is what the key can carry");
}
