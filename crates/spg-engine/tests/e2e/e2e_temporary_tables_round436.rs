//! read01 round 436 — TEMPORARY tables.
//!
//! `CREATE TEMPORARY TABLE` (MySQL) and `CREATE TEMP TABLE` (PG, where it is
//! everyday usage) were **consumed and answered OK while creating nothing**.
//! The DDL itself lied: every statement that touched the table afterwards
//! failed with "relation does not exist", and no amount of reading the
//! CREATE's reply would tell you why.
//!
//! Measured on MariaDB 11 and implemented:
//!   * a temp table SHADOWS a permanent one of the same name
//!   * dropping the temp reveals the permanent one again
//!   * `CREATE TEMPORARY TABLE … AS SELECT` works (CTAS shape)
//!   * a plain `DROP TABLE t` drops the TEMPORARY one, not the permanent
//!   * a duplicate temp name errors
//!
//! Plus the two session rules both engines share: another session never sees
//! it, and it dies with the session.
//!
//! Implementation note — a temp table is stored under a per-session name
//! prefix and resolved at `Catalog::resolve_index`, the ONE place a table
//! name becomes an index. That is PG's own `pg_temp` search-path rule, and
//! it is why ~170 engine call sites needed no change.

use spg_engine::{Engine, QueryResult};

fn cells(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>()
            .join(","),
        other => panic!("{sql}: {other:?}"),
    }
}

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

#[test]
fn round436_temp_table_is_created_and_usable() {
    let mut e = mysql();
    e.execute("CREATE TEMPORARY TABLE tmp(a INT, b VARCHAR(5))")
        .unwrap();
    e.execute("INSERT INTO tmp VALUES (1,'x')").unwrap();
    assert_eq!(cells(&mut e, "SELECT a,b FROM tmp"), "1|x");
    e.execute("UPDATE tmp SET b='y' WHERE a=1").unwrap();
    assert_eq!(cells(&mut e, "SELECT b FROM tmp"), "y");
    e.execute("DELETE FROM tmp").unwrap();
    assert_eq!(cells(&mut e, "SELECT COUNT(*) FROM tmp"), "0");
}

#[test]
fn round436_temp_shadows_a_permanent_table_of_the_same_name() {
    let mut e = mysql();
    e.execute("CREATE TABLE t(i INT)").unwrap();
    e.execute("INSERT INTO t VALUES (1),(2)").unwrap();
    e.execute("CREATE TEMPORARY TABLE t(i INT)").unwrap();
    e.execute("INSERT INTO t VALUES (99)").unwrap();
    // Reads and writes go to the temp one…
    assert_eq!(cells(&mut e, "SELECT i FROM t"), "99");
    // …and dropping it reveals the permanent one, untouched.
    e.execute("DROP TEMPORARY TABLE t").unwrap();
    assert_eq!(cells(&mut e, "SELECT i FROM t ORDER BY i"), "1,2");
}

#[test]
fn round436_plain_drop_table_drops_the_temporary_one() {
    // Measured: MariaDB's `DROP TABLE t` with both a temp and a permanent
    // `t` in play removes the temp. Dropping the permanent one instead
    // would have destroyed data every other session can see.
    let mut e = mysql();
    e.execute("CREATE TABLE t(i INT)").unwrap();
    e.execute("INSERT INTO t VALUES (1)").unwrap();
    e.execute("CREATE TEMPORARY TABLE t(i INT)").unwrap();
    e.execute("DROP TABLE t").unwrap();
    assert_eq!(cells(&mut e, "SELECT i FROM t"), "1");
}

#[test]
fn round436_create_temporary_table_as_select() {
    let mut e = mysql();
    e.execute("CREATE TABLE t(i INT)").unwrap();
    e.execute("INSERT INTO t VALUES (1),(2)").unwrap();
    e.execute("CREATE TEMPORARY TABLE tmp2 AS SELECT i FROM t")
        .unwrap();
    assert_eq!(cells(&mut e, "SELECT i FROM tmp2 ORDER BY i"), "1,2");
    // …and it really is temporary: a second one of the same name errors.
    e.execute("CREATE TEMPORARY TABLE tmp2(z INT)")
        .expect_err("duplicate temp name must error");
}

#[test]
fn round436_another_session_never_sees_it_and_it_dies_with_the_session() {
    let mut e = Engine::new();
    e.set_current_session(1);
    e.execute("CREATE TEMPORARY TABLE iso(a INT)").unwrap();
    e.execute("INSERT INTO iso VALUES (7)").unwrap();

    e.set_current_session(2);
    e.execute("SELECT a FROM iso")
        .expect_err("another session must not see the temp table");

    e.set_current_session(1);
    assert_eq!(cells(&mut e, "SELECT a FROM iso"), "7");

    // The owning session goes away; its temp tables go with it.
    e.set_current_session(2);
    e.end_session(1);
    e.set_current_session(1);
    e.execute("SELECT a FROM iso")
        .expect_err("the temp table must not outlive its session");
}

#[test]
fn round436_pg_dialect_create_temp_table_works_too() {
    // PG spells it TEMP and uses it constantly; it was the same silent no-op.
    let mut e = Engine::new();
    e.execute("CREATE TEMP TABLE pt(a INT)").unwrap();
    e.execute("INSERT INTO pt VALUES (5)").unwrap();
    assert_eq!(cells(&mut e, "SELECT a FROM pt"), "5");
}

#[test]
fn round436_a_session_without_temp_tables_resolves_normally() {
    // The prefix is only installed while the session owns a temp table, so
    // an ordinary workload keeps the plain lookup path.
    let mut e = mysql();
    e.execute("CREATE TABLE t(i INT)").unwrap();
    e.execute("INSERT INTO t VALUES (3)").unwrap();
    assert_eq!(cells(&mut e, "SELECT i FROM t"), "3");
    e.execute("CREATE TEMPORARY TABLE tmp(a INT)").unwrap();
    e.execute("DROP TABLE tmp").unwrap();
    // …and after the last one is dropped, still normal.
    assert_eq!(cells(&mut e, "SELECT i FROM t"), "3");
}
